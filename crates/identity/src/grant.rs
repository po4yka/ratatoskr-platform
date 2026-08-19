//! What an authenticated actor is allowed to do.
//!
//! `ARCHITECTURE.md` S7: authorization combines the authenticated actor, ownership, the requested
//! action and a capability. Milestone 2 built `identity.grants` for the capability half and left it
//! without a reader; milestone 8 needs one, because an OAuth relay has to name who may claim it and
//! **`sessions.audience` cannot be that name**.
//!
//! The audience of a session is the LISTENER it may be presented at — `edge`, `ingest` — so every
//! service talking to the public API holds a session with the same audience as every person does.
//! Binding a relay to an audience would therefore have bound it to nothing. A grant is the
//! mechanism that already exists for "this actor, this capability", and the vocabulary is open on
//! purpose (`migrations/0001_identity.sql`), so `oauth.claim.github` needs no schema change to
//! exist.

use sqlx::PgExecutor;
use uuid::Uuid;

use crate::PersistenceError;

/// Whether `user_id` currently holds `capability`.
///
/// Live means granted, not revoked, and either never expiring or not yet expired — the same
/// three-part liveness the partial unique index in `migrations/0001_identity.sql` is built around.
/// Evaluated in SQL rather than by reading the row and deciding here, so a caller cannot forget one
/// of the three.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn holds<'e, E>(
    executor: E,
    user_id: Uuid,
    capability: &str,
    now: jiff::Timestamp,
) -> Result<bool, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let held: bool = sqlx::query_scalar(
        "select exists (
             select 1 from identity.grants
              where user_id = $1
                and capability = $2
                and revoked_at is null
                and (expires_at is null or expires_at > $3)
         )",
    )
    .bind(user_id)
    .bind(capability)
    .bind(to_offset(now))
    .fetch_one(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(held)
}

/// Give `user_id` a capability, or refresh the grant it already has.
///
/// Idempotent on the live grant, which is what the partial unique index enforces: granting twice is
/// one grant, and a revoked one does not block a new one.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn grant<'e, E>(
    executor: E,
    user_id: Uuid,
    capability: &str,
    now: jiff::Timestamp,
    expires_at: Option<jiff::Timestamp>,
) -> Result<Uuid, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let grant_id = Uuid::now_v7();
    let stored: Uuid = sqlx::query_scalar(
        "insert into identity.grants (grant_id, user_id, capability, granted_at, expires_at)
         values ($1, $2, $3, $4, $5)
         on conflict (user_id, capability) where revoked_at is null
         do update set expires_at = excluded.expires_at
         returning grant_id",
    )
    .bind(grant_id)
    .bind(user_id)
    .bind(capability)
    .bind(to_offset(now))
    .bind(expires_at.map(to_offset))
    .fetch_one(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(stored)
}

/// Withdraw a capability. Returns whether anything was live to withdraw.
///
/// The row stays, with `revoked_at` set, so an audit reader can answer when it stopped applying.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn revoke<'e, E>(
    executor: E,
    user_id: Uuid,
    capability: &str,
    now: jiff::Timestamp,
) -> Result<bool, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let done = sqlx::query(
        "update identity.grants set revoked_at = $3
          where user_id = $1 and capability = $2 and revoked_at is null",
    )
    .bind(user_id)
    .bind(capability)
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(done.rows_affected() > 0)
}

/// `jiff` on the wire, `time` in the driver, through unix nanoseconds.
fn to_offset(instant: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(instant.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
