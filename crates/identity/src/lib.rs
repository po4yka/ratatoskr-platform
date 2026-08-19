//! The `identity` schema: who a principal is, and what still authenticates them.
//!
//! Milestone 2. This crate owns exactly the tables in `migrations/0001_identity.sql` and reaches no
//! other schema (`ARCHITECTURE.md` S19 invariant 6).
//!
//! Two rules shape every signature here.
//!
//! **A secret never crosses this boundary in a readable form.** Every credential is presented as a
//! 32-byte digest computed by the caller, and the digest type is [`SecretDigest`] rather than
//! `[u8; 32]` so a hash and an arbitrary byte string cannot be confused at a call site. Nothing in
//! this crate can return a credential, because no row it reads contains one.
//!
//! **Liveness is computed, never stored.** A session is live when it is unrevoked and unexpired,
//! evaluated against a caller-supplied instant. There is no `is_live` column to fall out of date,
//! and passing the instant in rather than reading the clock is what makes expiry testable.
//!
//! Queries are checked at run time, not by the `sqlx::query!` macros. The macros need either a live
//! database at compile time or a committed offline cache, and both make `cargo build` depend on
//! something that is not the source tree. The integration suite runs every statement in this crate
//! against a real `PostgreSQL`, so a wrong column name fails there instead. Revisit when CI has a
//! database service for the build job as well as the test job.

use platform_persistence::PersistenceError;
use sqlx::PgExecutor;
use sqlx::Row as _;
use uuid::Uuid;

pub mod assertion;
pub mod audit;
pub mod device;
pub mod grant;
pub mod relay;
pub mod session;
pub mod user;

pub use crate::assertion::{AssertionClaims, AssertionRejected};
pub use crate::audit::{AuditEvent, AuditOutcome};
pub use crate::device::{Device, DeviceKind};
pub use crate::relay::{CallbackOutcome, ClaimedRelay};
pub use crate::session::{NewSession, RefreshToken, Session, SessionKind};
pub use crate::user::{ExternalIdentity, IdentityProvider, User, UserStatus};

/// A 32-byte digest of a credential.
///
/// The database CHECK constraints require exactly this length, so a plaintext secret, which is
/// printable and a different length, cannot be stored by mistake. The type exists so that the
/// constraint is also expressed in Rust and not only in SQL.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SecretDigest([u8; 32]);

impl SecretDigest {
    /// Wrap a digest the caller computed.
    #[must_use]
    pub const fn new(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// The digest of a presented credential.
    ///
    /// The one place a credential is hashed. Two surfaces authenticate against a stored digest —
    /// a session on the client listener and a webhook source on the ingest listener — and a second
    /// implementation of "hash the credential" is a way for them to disagree about what a
    /// credential is.
    ///
    /// SHA-256, and deliberately not a password hash: the credential is a 256-bit random value we
    /// minted, not a secret a person chose, so there is no low-entropy guess for a slow hash to
    /// defend against.
    #[must_use]
    pub fn of(credential: &str) -> Self {
        Self(<sha2::Sha256 as sha2::Digest>::digest(credential.as_bytes()).into())
    }

    /// The bytes, for the one place that writes them into a `bytea` column.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for SecretDigest {
    /// A digest is not a secret, but printing one into a log lets an attacker who later obtains the
    /// log confirm a guess offline. There is no reason to render it at all.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SecretDigest([REDACTED])")
    }
}

/// The digest of the credential in `Authorization: Bearer …`, when there is one.
///
/// The scheme is matched case-insensitively, per RFC 9110 11.1. The credential is hashed
/// immediately and the plaintext is never returned, so no caller can hold it, log it, or pass it
/// on. Shared by both authenticating surfaces for the same reason [`SecretDigest::of`] is: two
/// parsers is two chances to disagree about what counts as a credential.
#[must_use]
pub fn bearer(headers: &http::HeaderMap) -> Option<SecretDigest> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, credential) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let credential = credential.trim();
    if credential.is_empty() {
        return None;
    }
    Some(SecretDigest::of(credential))
}

/// Why a credential stopped being valid.
///
/// The closed vocabulary of `identity.revocations.reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationReason {
    /// The user asked.
    UserRequest,
    /// An operator acted.
    Administrative,
    /// Routine rotation replaced it.
    CredentialRotation,
    /// It may have leaked.
    SuspectedCompromise,
    /// A retention or lifetime policy ended it.
    ExpiryPolicy,
}

impl RevocationReason {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserRequest => "user_request",
            Self::Administrative => "administrative",
            Self::CredentialRotation => "credential_rotation",
            Self::SuspectedCompromise => "suspected_compromise",
            Self::ExpiryPolicy => "expiry_policy",
        }
    }
}

/// What was revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationSubject {
    /// Every credential of a user.
    User,
    /// One session.
    Session,
    /// One registered device.
    Device,
    /// One refresh token.
    RefreshToken,
}

impl RevocationSubject {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Session => "session",
            Self::Device => "device",
            Self::RefreshToken => "refresh_token",
        }
    }
}

/// Record why a credential was revoked.
///
/// The `revoked_at` column on the subject row is the fast path an authentication check reads; this
/// is the durable why-and-by-whom that outlives the subject.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn record_revocation<'e, E>(
    executor: E,
    subject: RevocationSubject,
    subject_id: Uuid,
    reason: RevocationReason,
    revoked_by: Option<Uuid>,
    revoked_at: jiff::Timestamp,
) -> Result<Uuid, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let revocation_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.revocations
             (revocation_id, subject_kind, subject_id, reason, revoked_at, revoked_by)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(revocation_id)
    .bind(subject.as_str())
    .bind(subject_id)
    .bind(reason.as_str())
    .bind(crate::to_offset(revoked_at))
    .bind(revoked_by)
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(revocation_id)
}

/// Count the revocations recorded for one subject.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn count_revocations<'e, E>(
    executor: E,
    subject: RevocationSubject,
    subject_id: Uuid,
) -> Result<i64, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select count(*) as count from identity.revocations
          where subject_kind = $1 and subject_id = $2",
    )
    .bind(subject.as_str())
    .bind(subject_id)
    .fetch_one(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.try_get::<i64, _>("count")
        .map_err(PersistenceError::Query)
}

/// Convert a contracts wire instant into the type `sqlx` writes to `timestamptz`.
///
/// Through unix nanoseconds, which needs no calendar and cannot be ambiguous. A conversion that
/// cannot be represented is clamped rather than panicking: the workspace lints forbid `panic!` in
/// production code, and no timestamp this system mints is anywhere near the boundary.
pub(crate) fn to_offset(timestamp: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(timestamp.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

/// Convert a `timestamptz` read back out into the contracts wire instant.
pub(crate) fn from_offset(value: time::OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::from_nanosecond(value.unix_timestamp_nanos())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}
