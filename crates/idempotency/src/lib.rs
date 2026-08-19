//! The idempotency ledger.
//!
//! `ARCHITECTURE.md` S8.1 fixes the whole contract: the key is scoped by actor, route and operation
//! kind; retrying with the same payload returns the original operation; reusing the key with a
//! different payload is rejected.
//!
//! The reservation is taken in the CALLER'S transaction, alongside the operation and the outbox row.
//! That is what makes the three consistent: a crash between reserving and creating the operation
//! cannot leave a key claimed for work that never started, because there is no "between".
//!
//! Neither the key nor the request body is stored. Both are digests: a client-chosen
//! `Idempotency-Key` may carry meaning the client considers private, and this table is read by
//! operators.

use platform_persistence::PersistenceError;
use sha2::{Digest as _, Sha256};
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

/// A SHA-256 digest of something that must not be stored in the clear.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Hash a client-supplied `Idempotency-Key`.
    #[must_use]
    pub fn of_key(key: &str) -> Self {
        Self(Sha256::digest(key.as_bytes()).into())
    }

    /// Hash a request body, to detect the same key used for different work.
    #[must_use]
    pub fn of_body(body: &[u8]) -> Self {
        Self(Sha256::digest(body).into())
    }

    /// The bytes, for the one place that writes them into a `bytea` column.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for Digest {
    /// Never rendered. A key digest confirms a guess offline for anyone who later reads the log.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Digest([REDACTED])")
    }
}

/// What a reservation attempt found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    /// The key is new in this scope. The caller performs the work and then calls [`complete`].
    Fresh {
        /// The ledger row, to be completed once the answer is known.
        record_id: Uuid,
    },

    /// The same key and the same payload, already answered. The caller replays this answer instead
    /// of doing the work again. S8.1: "Retrying the same payload returns the original operation."
    Replay {
        /// The operation the first attempt created, when it created one.
        operation_id: Option<Uuid>,
        /// The status the first attempt returned.
        response_status: u16,
    },

    /// The same key and the same payload, but the first attempt has not finished. Two concurrent
    /// retries of one request land here; the caller answers 409 rather than starting a second
    /// operation, because starting one would be exactly the duplication the key exists to prevent.
    InFlight,

    /// The same key with a DIFFERENT payload. S8.1 rejects this outright: honouring it would let a
    /// client silently replace the meaning of a request it already sent.
    Conflict,
}

/// What a reservation means for the handler that took it.
///
/// Three outcomes rather than [`Reservation`]'s four, because a handler treats `InFlight` and
/// `Conflict` identically — both refuse — while the reasons differ only in the log. Kept here
/// rather than beside one route so the two route families that reserve keys cannot answer the same
/// reservation differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Do the work, then [`complete`] this ledger row.
    Proceed(Uuid),
    /// Answer with this operation. The first attempt already created it, so doing the work again
    /// would be the duplication the key exists to prevent.
    Replay(Uuid),
    /// Refuse with [`platform_core::FailureKind::IdempotencyConflict`].
    Refuse,
}

impl Reservation {
    /// What the caller should do about it.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        match *self {
            Self::Fresh { record_id } => Outcome::Proceed(record_id),
            Self::Replay {
                operation_id: Some(operation_id),
                ..
            } => Outcome::Replay(operation_id),
            // A completed reservation with no operation means the first attempt was refused before
            // it created one. Replaying its refusal is more truthful than starting work the first
            // attempt declined to start.
            Self::Replay { .. } | Self::InFlight | Self::Conflict => Outcome::Refuse,
        }
    }
}

/// Reserve a key inside the caller's transaction.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails.
#[expect(
    clippy::too_many_arguments,
    reason = "every argument is one column of the scope ARCHITECTURE.md S8.1 defines; a struct here \
              would be destructured at the only call site and hide which parts form the scope"
)]
pub async fn reserve(
    transaction: &mut sqlx::PgTransaction<'_>,
    owner_user_id: Uuid,
    route: &str,
    operation_kind: &str,
    key: Digest,
    fingerprint: Digest,
    now: jiff::Timestamp,
    ttl: jiff::SignedDuration,
) -> Result<Reservation, PersistenceError> {
    // An expired reservation is not a reservation. Deleting it first means the insert below either
    // succeeds cleanly or collides with a LIVE row, so the caller never has to reason about whether
    // a conflict it was told about is still in force.
    sqlx::query(
        "delete from operations.idempotency_records
          where owner_user_id = $1 and route = $2 and operation_kind = $3 and key_hash = $4
            and expires_at <= $5",
    )
    .bind(owner_user_id)
    .bind(route)
    .bind(operation_kind)
    .bind(key.as_bytes().as_slice())
    .bind(to_offset(now))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let record_id = Uuid::now_v7();
    let inserted = sqlx::query(
        "insert into operations.idempotency_records
             (record_id, owner_user_id, route, operation_kind, key_hash, request_fingerprint,
              reserved_at, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8)
         on conflict (owner_user_id, route, operation_kind, key_hash) do nothing",
    )
    .bind(record_id)
    .bind(owner_user_id)
    .bind(route)
    .bind(operation_kind)
    .bind(key.as_bytes().as_slice())
    .bind(fingerprint.as_bytes().as_slice())
    .bind(to_offset(now))
    .bind(to_offset(now + ttl))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    if inserted.rows_affected() == 1 {
        return Ok(Reservation::Fresh { record_id });
    }

    // Somebody holds it. `for update` serialises two concurrent retries of the same request, so the
    // second waits for the first's transaction rather than reading a half-written row.
    let row = sqlx::query(
        "select request_fingerprint, operation_id, response_status, completed_at
           from operations.idempotency_records
          where owner_user_id = $1 and route = $2 and operation_kind = $3 and key_hash = $4
          for update",
    )
    .bind(owner_user_id)
    .bind(route)
    .bind(operation_kind)
    .bind(key.as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let stored: Vec<u8> = row
        .try_get("request_fingerprint")
        .map_err(PersistenceError::Query)?;
    if stored.as_slice() != fingerprint.as_bytes().as_slice() {
        return Ok(Reservation::Conflict);
    }

    let completed_at: Option<time::OffsetDateTime> = row
        .try_get("completed_at")
        .map_err(PersistenceError::Query)?;
    if completed_at.is_none() {
        return Ok(Reservation::InFlight);
    }

    let status: Option<i16> = row
        .try_get("response_status")
        .map_err(PersistenceError::Query)?;
    Ok(Reservation::Replay {
        operation_id: row
            .try_get("operation_id")
            .map_err(PersistenceError::Query)?,
        // The schema's `completion_is_whole` constraint makes a completed row without a status
        // unreachable; 500 is the answer that is safe to replay if one ever appeared.
        response_status: status.and_then(|s| u16::try_from(s).ok()).unwrap_or(500),
    })
}

/// Record the answer, so a later retry replays it.
///
/// Called in the same transaction as the work it describes.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn complete<'e, E>(
    executor: E,
    record_id: Uuid,
    operation_id: Option<Uuid>,
    response_status: u16,
    now: jiff::Timestamp,
) -> Result<(), PersistenceError>
where
    E: PgExecutor<'e>,
{
    sqlx::query(
        "update operations.idempotency_records
            set operation_id = $2, response_status = $3, completed_at = $4
          where record_id = $1",
    )
    .bind(record_id)
    .bind(operation_id)
    .bind(i16::try_from(response_status).unwrap_or(500))
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(())
}

/// Delete reservations whose window has closed.
///
/// Returns how many were collected. `DATA_MODEL.md` gives the idempotency window its own retention
/// class; without collection the ledger grows for the life of the deployment.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn collect_expired<'e, E>(
    executor: E,
    now: jiff::Timestamp,
) -> Result<u64, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let result = sqlx::query("delete from operations.idempotency_records where expires_at <= $1")
        .bind(to_offset(now))
        .execute(executor)
        .await
        .map_err(PersistenceError::Query)?;
    Ok(result.rows_affected())
}

fn to_offset(value: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
