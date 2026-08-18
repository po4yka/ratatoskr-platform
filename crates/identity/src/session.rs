//! Sessions, their rotating refresh credentials, and the assertions that mint them.
//!
//! `ARCHITECTURE.md` S6.2 gives each session kind its own audience, lifetime, rotation and revocation
//! semantics, so [`SessionKind`] and the audience are separate values here rather than one
//! conflated string.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{SecretDigest, from_offset, to_offset};

/// How a session was established, which fixes its rotation and revocation semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A browser session backed by a cookie.
    Browser,
    /// A registered mobile or extension device.
    Device,
    /// A short-lived Telegram Mini App session.
    TelegramMiniApp,
    /// Service-to-service identity.
    Service,
    /// A personal API token.
    ApiToken,
}

impl SessionKind {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Device => "device",
            Self::TelegramMiniApp => "telegram_mini_app",
            Self::Service => "service",
            Self::ApiToken => "api_token",
        }
    }

    /// Parse a stored token.
    #[must_use]
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "browser" => Some(Self::Browser),
            "device" => Some(Self::Device),
            "telegram_mini_app" => Some(Self::TelegramMiniApp),
            "service" => Some(Self::Service),
            "api_token" => Some(Self::ApiToken),
            _ => None,
        }
    }

    /// Whether this kind must be bound to a registered device.
    ///
    /// Mirrors the `sessions_device_kind_has_a_device` CHECK constraint, so the rule is stated in
    /// both places and a test asserts they agree.
    #[must_use]
    pub const fn requires_device(self) -> bool {
        matches!(self, Self::Device)
    }
}

/// A revocable authentication state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The session's identity.
    pub session_id: Uuid,
    /// Its owner.
    pub user_id: Uuid,
    /// How it was established.
    pub kind: SessionKind,
    /// The device it is bound to, for a device session.
    pub device_id: Option<Uuid>,
    /// The audience it is valid for. An audience mismatch is an authentication failure, not a
    /// warning (`SECURITY.md`, "validate issuer/audience/expiry/nonce").
    pub audience: String,
    /// When it was issued.
    pub issued_at: jiff::Timestamp,
    /// When it stops being valid on its own.
    pub expires_at: jiff::Timestamp,
    /// When it was revoked ahead of expiry, if it was.
    pub revoked_at: Option<jiff::Timestamp>,
}

impl Session {
    /// Whether this session authenticates a request at `now`.
    ///
    /// Computed, never stored. The instant is a parameter so expiry is testable without waiting.
    #[must_use]
    pub fn is_live_at(&self, now: jiff::Timestamp) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

/// A rotating refresh credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshToken {
    /// The token's identity. Not the credential.
    pub token_id: Uuid,
    /// The session it refreshes.
    pub session_id: Uuid,
    /// When it was issued.
    pub issued_at: jiff::Timestamp,
    /// When it stops being usable.
    pub expires_at: jiff::Timestamp,
    /// When it was spent, if it was.
    pub consumed_at: Option<jiff::Timestamp>,
    /// The token that replaced it, if it was rotated.
    pub replaced_by: Option<Uuid>,
}

/// What went wrong when presenting a refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefreshFailure {
    /// No token matches the presented digest.
    #[error("no such refresh token")]
    Unknown,
    /// The token is past its expiry.
    #[error("the refresh token has expired")]
    Expired,
    /// The token was already spent. ARCHITECTURE's threat model calls this replay; it is evidence
    /// that a credential leaked, because a well-behaved client never presents a spent token.
    #[error("the refresh token was already used")]
    Replayed,
    /// The session it belongs to is revoked or expired.
    #[error("the session is no longer live")]
    SessionNotLive,
}

/// Open a session.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails, including when the device binding does not
/// satisfy the schema's rule for the kind.
pub async fn create_session<'e, E>(
    executor: E,
    user_id: Uuid,
    kind: SessionKind,
    device_id: Option<Uuid>,
    audience: &str,
    issued_at: jiff::Timestamp,
    expires_at: jiff::Timestamp,
) -> Result<Session, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let session_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.sessions
             (session_id, user_id, kind, device_id, audience, issued_at, expires_at, last_seen_at)
         values ($1, $2, $3, $4, $5, $6, $7, $6)",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(kind.as_str())
    .bind(device_id)
    .bind(audience)
    .bind(to_offset(issued_at))
    .bind(to_offset(expires_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(Session {
        session_id,
        user_id,
        kind,
        device_id,
        audience: audience.to_owned(),
        issued_at,
        expires_at,
        revoked_at: None,
    })
}

/// Read a session by identity.
///
/// Returns the row whatever its state: the caller decides what to do with a revoked or expired
/// session, and hiding it here would make a revoked session indistinguishable from a forged id.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn find_session<'e, E>(
    executor: E,
    session_id: Uuid,
) -> Result<Option<Session>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select session_id, user_id, kind, device_id, audience, issued_at, expires_at, revoked_at
           from identity.sessions where session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| {
        let kind: String = row.try_get("kind").map_err(PersistenceError::Query)?;
        let revoked_at: Option<time::OffsetDateTime> =
            row.try_get("revoked_at").map_err(PersistenceError::Query)?;
        Ok(Session {
            session_id: row.try_get("session_id").map_err(PersistenceError::Query)?,
            user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
            // An unknown token cannot pass the CHECK constraint. `Service` is the safe fallback:
            // it is the kind with no device binding and no client-facing rotation.
            kind: SessionKind::from_str_opt(&kind).unwrap_or(SessionKind::Service),
            device_id: row.try_get("device_id").map_err(PersistenceError::Query)?,
            audience: row.try_get("audience").map_err(PersistenceError::Query)?,
            issued_at: from_offset(row.try_get("issued_at").map_err(PersistenceError::Query)?),
            expires_at: from_offset(row.try_get("expires_at").map_err(PersistenceError::Query)?),
            revoked_at: revoked_at.map(from_offset),
        })
    })
    .transpose()
}

/// Revoke one session.
///
/// Idempotent: revoking an already-revoked session keeps the first instant, because the first one
/// is the one the audit trail and any incident timeline care about.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn revoke_session<'e, E>(
    executor: E,
    session_id: Uuid,
    revoked_at: jiff::Timestamp,
) -> Result<bool, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let result = sqlx::query(
        "update identity.sessions set revoked_at = $2
          where session_id = $1 and revoked_at is null",
    )
    .bind(session_id)
    .bind(to_offset(revoked_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(result.rows_affected() == 1)
}

/// Revoke every live session of a user.
///
/// The blast radius of a suspected compromise. Returns how many were revoked.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn revoke_all_sessions_of_user<'e, E>(
    executor: E,
    user_id: Uuid,
    revoked_at: jiff::Timestamp,
) -> Result<u64, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let result = sqlx::query(
        "update identity.sessions set revoked_at = $2
          where user_id = $1 and revoked_at is null",
    )
    .bind(user_id)
    .bind(to_offset(revoked_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(result.rows_affected())
}

/// Issue a refresh token for a session.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails.
pub async fn issue_refresh_token<'e, E>(
    executor: E,
    session_id: Uuid,
    digest: SecretDigest,
    issued_at: jiff::Timestamp,
    expires_at: jiff::Timestamp,
) -> Result<RefreshToken, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let token_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.refresh_tokens
             (token_id, session_id, token_hash, issued_at, expires_at)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(token_id)
    .bind(session_id)
    .bind(digest.as_bytes().as_slice())
    .bind(to_offset(issued_at))
    .bind(to_offset(expires_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(RefreshToken {
        token_id,
        session_id,
        issued_at,
        expires_at,
        consumed_at: None,
        replaced_by: None,
    })
}

/// Spend a refresh token and issue its successor, in one transaction.
///
/// The whole rotation is one statement pair inside the caller's transaction so that a crash between
/// them cannot leave a session with two live tokens or none.
///
/// Presenting a token that was already spent returns [`RefreshFailure::Replayed`] rather than
/// [`RefreshFailure::Unknown`], because the two mean very different things to an operator: the
/// second is a wrong guess, the first is a leaked credential in use.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails. A refusal is `Ok(Err(RefreshFailure))`: it is
/// an expected outcome of an authentication attempt, not a fault.
pub async fn rotate_refresh_token(
    transaction: &mut sqlx::PgTransaction<'_>,
    presented: SecretDigest,
    replacement: SecretDigest,
    now: jiff::Timestamp,
    expires_at: jiff::Timestamp,
) -> Result<Result<RefreshToken, RefreshFailure>, PersistenceError> {
    let row = sqlx::query(
        "select t.token_id, t.session_id, t.expires_at, t.consumed_at,
                s.revoked_at as session_revoked_at, s.expires_at as session_expires_at
           from identity.refresh_tokens t
           join identity.sessions s on s.session_id = t.session_id
          where t.token_hash = $1
          for update of t",
    )
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else {
        return Ok(Err(RefreshFailure::Unknown));
    };

    let token_id: Uuid = row.try_get("token_id").map_err(PersistenceError::Query)?;
    let session_id: Uuid = row.try_get("session_id").map_err(PersistenceError::Query)?;
    let token_expires: time::OffsetDateTime =
        row.try_get("expires_at").map_err(PersistenceError::Query)?;
    let consumed_at: Option<time::OffsetDateTime> = row
        .try_get("consumed_at")
        .map_err(PersistenceError::Query)?;
    let session_revoked: Option<time::OffsetDateTime> = row
        .try_get("session_revoked_at")
        .map_err(PersistenceError::Query)?;
    let session_expires: time::OffsetDateTime = row
        .try_get("session_expires_at")
        .map_err(PersistenceError::Query)?;

    if consumed_at.is_some() {
        return Ok(Err(RefreshFailure::Replayed));
    }
    if from_offset(token_expires) <= now {
        return Ok(Err(RefreshFailure::Expired));
    }
    if session_revoked.is_some() || from_offset(session_expires) <= now {
        return Ok(Err(RefreshFailure::SessionNotLive));
    }

    let successor = Uuid::now_v7();
    sqlx::query(
        "insert into identity.refresh_tokens
             (token_id, session_id, token_hash, issued_at, expires_at)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(successor)
    .bind(session_id)
    .bind(replacement.as_bytes().as_slice())
    .bind(to_offset(now))
    .bind(to_offset(expires_at))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    // Consumed and replaced in one statement. The schema's
    // `refresh_tokens_replacement_implies_consumption` CHECK makes the other order impossible,
    // which is what stops a successor existing while its predecessor is still usable.
    sqlx::query(
        "update identity.refresh_tokens
            set consumed_at = $2, replaced_by = $3
          where token_id = $1",
    )
    .bind(token_id)
    .bind(to_offset(now))
    .bind(successor)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(Ok(RefreshToken {
        token_id: successor,
        session_id,
        issued_at: now,
        expires_at,
        consumed_at: None,
        replaced_by: None,
    }))
}
