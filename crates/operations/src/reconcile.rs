//! The stale-operation reaper: `ARCHITECTURE.md` S14's reconciliation, ADR-0014's reading of it.
//!
//! An operation whose worker died — a crashed extractor, a lost progress event, a command nobody
//! consumed — otherwise stays `accepted` or `running` forever. This pass terminates such
//! operations as `failed` through the ONE transition applier, so a client polling the operation or
//! streaming its events sees a truthful end instead of an open one.
//!
//! Liveness is the newest observed fact about an operation — `greatest(status_changed_at,
//! max(operation_progress.observed_at))` — and not the status-change time alone, because
//! `status_changed_at` moves only on an applied ADVANCE: a worker reporting progress every minute
//! without ever changing status is alive, and harvesting on status age would kill healthy
//! long-running work on any threshold an operator could plausibly set.
//!
//! One transaction per candidate, re-verifying liveness under `FOR UPDATE` inside it — the same
//! shape the scheduler uses for its occurrences, for the same reason: selection without a lock
//! races a worker report, and the fresh fact must win.

use jiff::{SignedDuration, Timestamp};
use ratatoskr_operation_contracts::OperationStatus;
use sqlx::PgPool;
use uuid::Uuid;

use crate::OperationError;
use platform_persistence::PersistenceError;
use sqlx::Row as _;

/// The stable code every reconciled operation carries. Service-owned: the contracts grammar is
/// the contract, and this is Platform's own bounded context naming its own decision.
pub const STALE_ERROR_CODE: &str = "platform.operation.stale";

/// What both the progress history and the error record say, in user-safe words.
const STALE_MESSAGE: &str = "No updates were received for this operation, so it was marked failed. You can submit the request again.";

/// What one pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Operations terminated by this pass.
    pub reconciled: i64,
    /// Candidates whose re-check under lock found fresh facts; a worker got there first.
    pub skipped: i64,
}

/// Terminate every unterminated operation with no observed fact newer than `stale_after`.
///
/// At most `batch` operations are examined, oldest first; a backlog drains over successive passes,
/// which is what bounds the locks one pass can take on a database three services share.
///
/// # Errors
///
/// [`OperationError::Persistence`] if any statement fails.
pub async fn run_once(
    pool: &PgPool,
    stale_after: SignedDuration,
    batch: i64,
    now: Timestamp,
) -> Result<Report, OperationError> {
    let cutoff = to_offset(now - stale_after);
    let candidates = sqlx::query(
        "select o.operation_id
           from operations.operations o
          where o.terminated_at is null
            and o.status in ('accepted', 'queued', 'running')
            and greatest(o.status_changed_at,
                         coalesce((select max(p.observed_at)
                                     from operations.operation_progress p
                                    where p.operation_id = o.operation_id),
                                  o.status_changed_at)) < $1
          order by o.status_changed_at, o.operation_id
          limit $2",
    )
    .bind(cutoff)
    .bind(batch)
    .map(|row: sqlx::postgres::PgRow| row.get::<Uuid, _>("operation_id"))
    .fetch_all(pool)
    .await
    .map_err(PersistenceError::Query)?;

    let mut report = Report {
        reconciled: 0,
        skipped: 0,
    };
    for operation_id in candidates {
        if reconcile_one(pool, operation_id, cutoff, now).await? {
            metrics::counter!(platform_telemetry::metrics::PLATFORM_OPERATIONS_RECONCILED_TOTAL)
                .increment(1);
            report.reconciled += 1;
        } else {
            report.skipped += 1;
        }
    }
    Ok(report)
}

/// Terminate one operation, or report that it was still alive when locked.
async fn reconcile_one(
    pool: &PgPool,
    operation_id: Uuid,
    cutoff: time::OffsetDateTime,
    now: Timestamp,
) -> Result<bool, OperationError> {
    let mut transaction = pool.begin().await.map_err(PersistenceError::Query)?;

    // The predicate again, this time under the row lock, against the state that will be written
    // over: a progress entry committed between selection and here makes this return nothing, and
    // the worker's fact beats the reaper's arithmetic.
    let still_stale = sqlx::query_scalar::<_, Uuid>(
        "select operation_id
           from operations.operations
          where operation_id = $1
            and terminated_at is null
            and status in ('accepted', 'queued', 'running')
            and greatest(status_changed_at,
                         coalesce((select max(p.observed_at)
                                     from operations.operation_progress p
                                    where p.operation_id = operations.operations.operation_id),
                                  status_changed_at)) < $2
          for update",
    )
    .bind(operation_id)
    .bind(cutoff)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(PersistenceError::Query)?;

    if still_stale.is_none() {
        transaction
            .rollback()
            .await
            .map_err(PersistenceError::Query)?;
        return Ok(false);
    }

    // The one transition applier (ADR-0002). It appends the SSE-visible progress entry, counts the
    // transition metric, and sets `terminated_at`; the trigger behind the UPDATE enforces the same
    // rule a third time for any writer that bypasses all of this.
    crate::record_status(
        &mut transaction,
        operation_id,
        OperationStatus::Failed,
        None,
        None,
        Some(STALE_MESSAGE),
        now,
    )
    .await?;

    // An operation the PLATFORM terminated for silence may honestly be resubmitted, and invariant
    // I2 refuses a `failed` snapshot with no error — so the flag, the record and the status land in
    // one transaction or not at all.
    let error = ratatoskr_error_contracts::ErrorEnvelope::new(
        ratatoskr_error_contracts::ErrorCode::parse(STALE_ERROR_CODE)
            .map_err(|error| crate::OperationError::ContractViolation(error.to_string()))?,
        ratatoskr_identifiers::SafeMessage::parse(STALE_MESSAGE)
            .map_err(|error| crate::OperationError::ContractViolation(error.to_string()))?,
        true,
    );
    crate::record_error(&mut transaction, operation_id, &error, now).await?;

    transaction
        .commit()
        .await
        .map_err(PersistenceError::Query)?;
    Ok(true)
}

fn to_offset(value: Timestamp) -> time::OffsetDateTime {
    // The same convention as `lib.rs`: a timestamp outside the supported range saturates to the
    // epoch, which matches nothing and terminates nothing, rather than panicking inside a
    // background loop nobody is watching.
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
