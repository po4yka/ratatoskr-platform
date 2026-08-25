//! Recording that the owner of an operation asked for it to stop.
//!
//! The schema fixes the semantics on the column this module writes:
//! `cancellation_requested_at` is "a request, not a state" — the operation reaches `cancelled`
//! only when its owning service confirms it stopped, reporting through the same progress contract
//! as any other outcome. This module therefore never touches `status`; it records the request and
//! classifies the attempt against current truth so its caller can decide whether anything further
//! — an outgoing command, a response body — is warranted.
//!
//! One locked read decides everything: ownership, liveness, and whether a request already stands.
//! The row lock held across the decision is what makes the answer safe against the other writers —
//! [`crate::record_status`] and the stale-operation reaper both classify under the same lock — so
//! two readers of one operation can never interleave between "what is true?" and "what I did
//! about it".

use platform_persistence::PersistenceError;
use sqlx::Row as _;
use uuid::Uuid;

use crate::{Operation, OperationError};

/// What one cancellation attempt found.
///
/// The three outcomes are real answers, not errors. A live state records a request, a repeat finds
/// its own earlier request, and a terminal state answers with what already happened — cancelling a
/// finished operation is never a fault, because truth needs no repair.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Cancellation {
    /// The operation was live with no request standing; the marker is now recorded.
    Requested(Operation),
    /// A request was already recorded earlier; nothing new was written.
    AlreadyRequested(Operation),
    /// The operation had already finished; truth is returned untouched.
    Terminal(Operation),
}

/// Record that cancellation of one operation was requested by its owner.
///
/// Ownership is checked inside the same locked read that classifies the attempt, so there is no
/// window in which a foreign caller could observe or influence the row. A missing identifier and
/// somebody else's produce the same refusal, matching how the public read route treats them
/// (`ARCHITECTURE.md` S15: authorization before existence).
///
/// Runs inside the caller's transaction so whatever else acceptance means — typically one command
/// into the outbox — commits or not together with the marker.
///
/// # Errors
///
/// [`OperationError::NotFound`] if no operation with that identity belongs to that owner,
/// [`OperationError::Persistence`] if a statement fails.
pub async fn request_cancellation(
    transaction: &mut sqlx::PgTransaction<'_>,
    operation_id: Uuid,
    owner_user_id: Uuid,
    now: jiff::Timestamp,
) -> Result<Cancellation, OperationError> {
    let row = sqlx::query(
        "select operation_id, owner_user_id, kind, status, stage, progress_percent,
                correlation_id, retryable, cancellation_requested_at, accepted_at,
                status_changed_at, terminated_at
           from operations.operations
          where operation_id = $1 and owner_user_id = $2
          for update",
    )
    .bind(operation_id)
    .bind(owner_user_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else {
        return Err(OperationError::NotFound);
    };
    let operation = operation_from_row(&row)?;

    if crate::transition::is_terminal(operation.status) {
        return Ok(Cancellation::Terminal(operation));
    }
    if operation.cancellation_requested_at.is_some() {
        return Ok(Cancellation::AlreadyRequested(operation));
    }

    // Only the marker moves. `status` and `status_changed_at` stay untouched, which is also why
    // the transition-guard trigger never sees this statement: it fires only on those columns.
    sqlx::query(
        "update operations.operations set cancellation_requested_at = $2 where operation_id = $1",
    )
    .bind(operation_id)
    .bind(crate::to_offset(now))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(Cancellation::Requested(Operation {
        cancellation_requested_at: Some(now),
        ..operation
    }))
}

/// The stored shape of one row of `operations.operations`.
///
/// Shared with the listing module, which projects the same columns without the lock.
pub(crate) fn operation_from_row(row: &sqlx::postgres::PgRow) -> Result<Operation, OperationError> {
    let status: String = row.try_get("status").map_err(PersistenceError::Query)?;
    let requested_at: Option<time::OffsetDateTime> = row
        .try_get("cancellation_requested_at")
        .map_err(PersistenceError::Query)?;
    let terminated_at: Option<time::OffsetDateTime> = row
        .try_get("terminated_at")
        .map_err(PersistenceError::Query)?;

    Ok(Operation {
        operation_id: row
            .try_get("operation_id")
            .map_err(PersistenceError::Query)?,
        owner_user_id: row
            .try_get("owner_user_id")
            .map_err(PersistenceError::Query)?,
        kind: row.try_get("kind").map_err(PersistenceError::Query)?,
        status: crate::status_from_str(&status).ok_or_else(|| {
            OperationError::ContractViolation(format!("unknown stored status {status}"))
        })?,
        stage: row.try_get("stage").map_err(PersistenceError::Query)?,
        progress_percent: row
            .try_get("progress_percent")
            .map_err(PersistenceError::Query)?,
        correlation_id: row
            .try_get("correlation_id")
            .map_err(PersistenceError::Query)?,
        retryable: row.try_get("retryable").map_err(PersistenceError::Query)?,
        cancellation_requested_at: requested_at.map(crate::from_offset),
        accepted_at: crate::from_offset(
            row.try_get("accepted_at")
                .map_err(PersistenceError::Query)?,
        ),
        status_changed_at: crate::from_offset(
            row.try_get("status_changed_at")
                .map_err(PersistenceError::Query)?,
        ),
        terminated_at: terminated_at.map(crate::from_offset),
    })
}
