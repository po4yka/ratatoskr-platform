//! Internal users, and the external identities that map onto them.
//!
//! `ARCHITECTURE.md` S6.1: an internal user UUID is independent of a Telegram ID, a GitHub ID or an
//! email address. That independence is the whole point of the split between [`User`] and
//! [`ExternalIdentity`], and it is why no function here accepts a provider identifier as the
//! primary key of anything.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{from_offset, to_offset};

/// Whether a user may authenticate at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStatus {
    /// Normal.
    Active,
    /// Temporarily barred; rows are retained.
    Suspended,
    /// A tombstone. The row stays so that sessions, audit records and operation history remain
    /// readable for their own retention windows.
    Deleted,
}

impl UserStatus {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    /// Parse a stored token.
    #[must_use]
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    /// Whether a principal in this state may hold a live session.
    #[must_use]
    pub const fn may_authenticate(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// The provider that owns an external identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityProvider {
    /// A Telegram account, mapped through a `ratatoskr-telegram` assertion.
    Telegram,
    /// A GitHub account.
    GitHub,
    /// An email address the user proved control of.
    Email,
}

impl IdentityProvider {
    /// Every provider, so a caller that must enumerate them cannot miss one. The array length is
    /// the documented count, so adding a variant without extending this does not compile.
    pub const ALL: [Self; 3] = [Self::Telegram, Self::GitHub, Self::Email];

    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::GitHub => "github",
            Self::Email => "email",
        }
    }

    /// The provider a stored token or a path segment names, or `None`.
    ///
    /// `None` is what makes an attacker-chosen path segment a `404` rather than a row: the OAuth
    /// callback route takes its provider from the URL, and the set of providers is a vocabulary
    /// rather than configuration precisely so that it can be closed here.
    #[must_use]
    pub fn from_str_opt(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

impl core::fmt::Display for IdentityProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An internal user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The internal identity. `UUIDv7`, minted by Platform.
    pub user_id: Uuid,
    /// Whether the user may authenticate.
    pub status: UserStatus,
    /// When the record was created.
    pub created_at: jiff::Timestamp,
    /// When the record last changed.
    pub updated_at: jiff::Timestamp,
}

/// A provider identity mapped onto an internal user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    /// The mapping's own identity.
    pub identity_id: Uuid,
    /// The internal user it resolves to.
    pub user_id: Uuid,
    /// Which provider.
    pub provider: IdentityProvider,
    /// The provider-side identifier, opaque to Platform.
    pub external_id: String,
}

/// Create a user.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails.
pub async fn create_user<'e, E>(executor: E, now: jiff::Timestamp) -> Result<User, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let user_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.users (user_id, status, created_at, updated_at)
         values ($1, 'active', $2, $2)",
    )
    .bind(user_id)
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(User {
        user_id,
        status: UserStatus::Active,
        created_at: now,
        updated_at: now,
    })
}

/// Read a user by internal identity.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn find_user<'e, E>(executor: E, user_id: Uuid) -> Result<Option<User>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select user_id, status, created_at, updated_at
           from identity.users where user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| {
        let status: String = row.try_get("status").map_err(PersistenceError::Query)?;
        Ok(User {
            user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
            // The CHECK constraint makes an unknown token unreachable; treating it as `Deleted`
            // rather than erroring keeps a read path alive if one is ever added by a schema change this
            // binary predates, and `Deleted` is the choice that denies rather than grants.
            status: UserStatus::from_str_opt(&status).unwrap_or(UserStatus::Deleted),
            created_at: from_offset(row.try_get("created_at").map_err(PersistenceError::Query)?),
            updated_at: from_offset(row.try_get("updated_at").map_err(PersistenceError::Query)?),
        })
    })
    .transpose()
}

/// Change a user's status, and stamp `updated_at`.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn set_user_status<'e, E>(
    executor: E,
    user_id: Uuid,
    status: UserStatus,
    now: jiff::Timestamp,
) -> Result<bool, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let result =
        sqlx::query("update identity.users set status = $2, updated_at = $3 where user_id = $1")
            .bind(user_id)
            .bind(status.as_str())
            .bind(to_offset(now))
            .execute(executor)
            .await
            .map_err(PersistenceError::Query)?;

    Ok(result.rows_affected() == 1)
}

/// Attach a provider identity to a user, or return the existing mapping.
///
/// Idempotent on `(provider, external_id)`, which is the unique index `schema.sql` creates: the
/// same provider identity always resolves to the same internal user, and a second attempt from a
/// retried request does not create a second user.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn link_external_identity<'e, E>(
    executor: E,
    user_id: Uuid,
    provider: IdentityProvider,
    external_id: &str,
    now: jiff::Timestamp,
) -> Result<ExternalIdentity, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "insert into identity.identities
             (identity_id, user_id, provider, external_id, created_at, last_seen_at)
         values ($1, $2, $3, $4, $5, $5)
         on conflict (provider, external_id)
         do update set last_seen_at = excluded.last_seen_at
         returning identity_id, user_id",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(provider.as_str())
    .bind(external_id)
    .bind(to_offset(now))
    .fetch_one(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(ExternalIdentity {
        identity_id: row
            .try_get("identity_id")
            .map_err(PersistenceError::Query)?,
        // Deliberately the stored value, not the argument: on a conflict the mapping already
        // belongs to some user, and that user wins. Returning the caller's argument here would let
        // one provider account silently move between internal users.
        user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
        provider,
        external_id: external_id.to_owned(),
    })
}

/// Resolve a provider identity to its internal user.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn find_user_by_external_identity<'e, E>(
    executor: E,
    provider: IdentityProvider,
    external_id: &str,
) -> Result<Option<Uuid>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select user_id from identity.identities where provider = $1 and external_id = $2",
    )
    .bind(provider.as_str())
    .bind(external_id)
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| row.try_get("user_id").map_err(PersistenceError::Query))
        .transpose()
}
