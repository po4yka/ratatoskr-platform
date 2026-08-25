//! The `operations` schema: the durable record of user-visible asynchronous work.
//!
//! Milestone 3. This crate owns exactly the `operations` tables of `schema.sql`.
//!
//! The lifecycle rule lives in [`status`] and is enforced twice on purpose: here, so a caller gets a
//! typed refusal, and by a trigger in `schema.sql`, so an `UPDATE` from anywhere else is refused
//! too. A test asserts the two agree.
//!
//! [`snapshot`] projects a stored operation onto `ratatoskr_operation_contracts::OperationSnapshot`
//! and then calls its `validate`, so a row that violates a contract invariant is caught here rather
//! than on the wire.

use platform_persistence::PersistenceError;

pub use crate::cancel::{Cancellation, request_cancellation};
pub use crate::list::{ListScope, Page, list_operations};
pub use crate::projection::ProgressProjection;
use ratatoskr_error_contracts::{ErrorEnvelope, WarningEnvelope};
use ratatoskr_identifiers::{EntityRef, Extensions, OperationId, TenantRef, UserId, WireTimestamp};
use ratatoskr_operation_contracts::{
    OperationKind, OperationResultRef, OperationSnapshot, OperationStage, OperationStatus,
    ProgressPercent,
};
use sqlx::{PgExecutor, Row as _};

use crate::transition::Transition;
use uuid::Uuid;

pub mod cancel;
pub mod list;
pub mod projection;
pub mod reconcile;
pub mod transition;

/// A refusal that is an expected outcome rather than a fault.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OperationError {
    /// Two producers reported different terminal outcomes for one operation. ADR-0002 classifies
    /// this as an alarm rather than ordinary traffic: a duplicate and a late older status are
    /// normal under at-least-once delivery, but two disagreeing terminal claims are a defect.
    #[error("conflicting terminal outcomes for one operation: {current} and {incoming}")]
    ConflictingOutcome {
        /// The recorded outcome.
        current: &'static str,
        /// The outcome that arrived afterwards.
        incoming: &'static str,
    },

    /// No operation with that identity.
    #[error("no such operation")]
    NotFound,

    /// A stored row does not satisfy a contract invariant, so no snapshot can be emitted for it.
    /// This is a defect in whatever wrote the row, and it is caught before the value reaches a
    /// client rather than after.
    #[error("the stored operation does not satisfy the snapshot contract")]
    ContractViolation(String),

    /// The database refused or failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// The stored operation, before projection.
#[derive(Debug, Clone)]
pub struct Operation {
    /// Its identity.
    pub operation_id: Uuid,
    /// The user it belongs to.
    pub owner_user_id: Uuid,
    /// What work it performs.
    pub kind: String,
    /// Where it is in the lifecycle.
    pub status: OperationStatus,
    /// A producer-defined display phase.
    pub stage: Option<String>,
    /// Optional bounded progress.
    pub progress_percent: Option<i16>,
    /// The namespaced correlation identifier.
    pub correlation_id: String,
    /// Whether the client may resubmit.
    pub retryable: bool,
    /// When the owner asked for the work to stop, if it was asked. A request, not a state: the
    /// operation reaches `cancelled` only when its owning service confirms it stopped.
    pub cancellation_requested_at: Option<jiff::Timestamp>,
    /// When it was accepted.
    pub accepted_at: jiff::Timestamp,
    /// When its status last changed.
    pub status_changed_at: jiff::Timestamp,
    /// When it finished, if it has.
    pub terminated_at: Option<jiff::Timestamp>,
}

pub(crate) fn status_str(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Accepted => "accepted",
        OperationStatus::Queued => "queued",
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::PartiallySucceeded => "partially_succeeded",
        OperationStatus::Failed => "failed",
        OperationStatus::Cancelled => "cancelled",
        // `OperationStatus` is `#[non_exhaustive]`, so a contracts release may add a variant this
        // binary predates. The token below fails the schema's CHECK constraint, which is the
        // intended outcome: an unrecognised lifecycle state must be a loud write failure, never a
        // silently stored value.
        _ => "unknown",
    }
}

pub(crate) fn status_from_str(value: &str) -> Option<OperationStatus> {
    match value {
        "accepted" => Some(OperationStatus::Accepted),
        "queued" => Some(OperationStatus::Queued),
        "running" => Some(OperationStatus::Running),
        "succeeded" => Some(OperationStatus::Succeeded),
        "partially_succeeded" => Some(OperationStatus::PartiallySucceeded),
        "failed" => Some(OperationStatus::Failed),
        "cancelled" => Some(OperationStatus::Cancelled),
        _ => None,
    }
}

pub(crate) fn to_offset(value: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

pub(crate) fn from_offset(value: time::OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::from_nanosecond(value.unix_timestamp_nanos())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// Accept an operation.
///
/// It starts in `accepted`, which ARCHITECTURE S5.1 defines as durable acceptance — the state the
/// `202 Accepted` response is allowed to describe, and nothing more.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the insert fails, including a duplicate idempotency key
/// within one owner and kind.
pub async fn accept<'e, E>(
    executor: E,
    owner_user_id: Uuid,
    kind: &str,
    correlation_id: &str,
    idempotency_key: Option<&str>,
    now: jiff::Timestamp,
) -> Result<Operation, OperationError>
where
    E: PgExecutor<'e>,
{
    let operation_id = Uuid::now_v7();
    sqlx::query(
        "insert into operations.operations
             (operation_id, owner_user_id, kind, status, correlation_id, idempotency_key,
              accepted_at, status_changed_at)
         values ($1, $2, $3, 'accepted', $4, $5, $6, $6)",
    )
    .bind(operation_id)
    .bind(owner_user_id)
    .bind(kind)
    .bind(correlation_id)
    .bind(idempotency_key)
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(Operation {
        operation_id,
        owner_user_id,
        kind: kind.to_owned(),
        status: OperationStatus::Accepted,
        stage: None,
        progress_percent: None,
        correlation_id: correlation_id.to_owned(),
        retryable: false,
        cancellation_requested_at: None,
        accepted_at: now,
        status_changed_at: now,
        terminated_at: None,
    })
}

/// Read one operation.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the statement fails.
pub async fn find<'e, E>(
    executor: E,
    operation_id: Uuid,
) -> Result<Option<Operation>, OperationError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select operation_id, owner_user_id, kind, status, stage, progress_percent,
                correlation_id, retryable, cancellation_requested_at,
                accepted_at, status_changed_at, terminated_at
           from operations.operations where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else { return Ok(None) };
    let status: String = row.try_get("status").map_err(PersistenceError::Query)?;
    let terminated_at: Option<time::OffsetDateTime> = row
        .try_get("terminated_at")
        .map_err(PersistenceError::Query)?;
    let requested_at: Option<time::OffsetDateTime> = row
        .try_get("cancellation_requested_at")
        .map_err(PersistenceError::Query)?;

    Ok(Some(Operation {
        operation_id: row
            .try_get("operation_id")
            .map_err(PersistenceError::Query)?,
        owner_user_id: row
            .try_get("owner_user_id")
            .map_err(PersistenceError::Query)?,
        kind: row.try_get("kind").map_err(PersistenceError::Query)?,
        status: status_from_str(&status).ok_or_else(|| {
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
        cancellation_requested_at: requested_at.map(from_offset),
        accepted_at: from_offset(
            row.try_get("accepted_at")
                .map_err(PersistenceError::Query)?,
        ),
        status_changed_at: from_offset(
            row.try_get("status_changed_at")
                .map_err(PersistenceError::Query)?,
        ),
        terminated_at: terminated_at.map(from_offset),
    }))
}

/// Apply an incoming status to an operation.
///
/// The classification comes from [`transition::apply`], which is the single authority ADR-0002
/// fixed. All four of its outcomes are real and the caller sees which one happened:
///
/// * `Advance` writes the new status and appends a progress entry.
/// * `Duplicate` and `Stale` write nothing. They are ordinary traffic under at-least-once delivery,
///   not failures, and the caller counts them.
/// * `Conflict` is [`OperationError::ConflictingOutcome`].
///
/// The database trigger refuses the same backward and terminal moves, which is what protects the
/// invariant from a writer that does not come through here.
///
/// # Errors
///
/// [`OperationError::NotFound`] if there is no such operation,
/// [`OperationError::ConflictingOutcome`] on two disagreeing terminal outcomes,
/// [`OperationError::Persistence`] if a statement fails.
pub async fn record_status(
    transaction: &mut sqlx::PgTransaction<'_>,
    operation_id: Uuid,
    incoming: OperationStatus,
    stage: Option<&str>,
    progress_percent: Option<u8>,
    message: Option<&str>,
    now: jiff::Timestamp,
) -> Result<(Transition, Operation), OperationError> {
    let current = find(&mut **transaction, operation_id)
        .await?
        .ok_or(OperationError::NotFound)?;

    let outcome = transition::apply(current.status, incoming);
    count_transition(outcome);
    let advance_to = match outcome {
        Transition::Advance(status) => status,
        Transition::Duplicate | Transition::Stale => return Ok((outcome, current)),
        Transition::Conflict => {
            return Err(OperationError::ConflictingOutcome {
                current: status_str(current.status),
                incoming: status_str(incoming),
            });
        }
    };

    let terminated_at = transition::is_terminal(advance_to).then(|| to_offset(now));

    sqlx::query(
        "update operations.operations
            set status = $2, stage = coalesce($3, stage), progress_percent = coalesce($4, progress_percent),
                status_changed_at = $5, terminated_at = $6
          where operation_id = $1",
    )
    .bind(operation_id)
    .bind(status_str(advance_to))
    .bind(stage)
    .bind(progress_percent.map(i16::from))
    .bind(to_offset(now))
    .bind(terminated_at)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    sqlx::query(
        "insert into operations.operation_progress
             (progress_id, operation_id, observed_at, status, stage, progress_percent, message)
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::now_v7())
    .bind(operation_id)
    .bind(to_offset(now))
    .bind(status_str(advance_to))
    .bind(stage)
    .bind(progress_percent.map(i16::from))
    .bind(message)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let updated = find(&mut **transaction, operation_id)
        .await?
        .ok_or(OperationError::NotFound)?;
    Ok((outcome, updated))
}

/// Count one transition, by what it meant.
///
/// `transition.rs` has described `Duplicate` as "a no-op plus a counter" and `Stale` as "ignored
/// plus a counter" since milestone 3, and there was no counter. This is it, and it is here rather
/// than in the caller because `record_status` is the only place `transition::apply` is consulted
/// about a real operation.
///
/// `conflict` is the value to alarm on. A duplicate and a late older status are ordinary traffic
/// under at-least-once delivery; two producers reporting different terminal outcomes for one
/// operation is a defect that ADR-0002 refuses to absorb silently.
fn count_transition(outcome: Transition) {
    let label = match outcome {
        Transition::Advance(_) => "advance",
        Transition::Duplicate => "duplicate",
        Transition::Stale => "stale",
        Transition::Conflict => "conflict",
    };
    metrics::counter!(
        platform_telemetry::metrics::PLATFORM_OPERATION_TRANSITIONS_TOTAL,
        "outcome" => label,
    )
    .increment(1);
}

/// Sample the operation table and publish its gauges.
///
/// `ARCHITECTURE.md` S16 item 3 asks for operation AGE as well as transition counts, and an age is
/// not knowable from any single write: it is a property of the set. So it is sampled on the
/// observer's timer, in one statement that scans only the unterminated rows.
///
/// The age is what the stale-operation reaper acts on (ADR-0014, `reconcile`): before that
/// existed, this gauge was the only way to see the condition it repairs.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the aggregate fails.
#[expect(
    clippy::cast_precision_loss,
    reason = "row counts and an age in seconds, exported as f64 gauges"
)]
pub async fn sample(pool: &sqlx::PgPool, now: jiff::Timestamp) -> Result<(), OperationError> {
    let rows = sqlx::query(
        "select status, count(*)::bigint as total,
                coalesce(max(extract(epoch from $1 - accepted_at)), 0)::bigint as oldest
           from operations.operations
          group by status",
    )
    .bind(to_offset(now))
    .fetch_all(pool)
    .await
    .map_err(PersistenceError::Query)?;

    // Every status is published, including the ones with no rows, so a series does not disappear
    // from a dashboard the moment the condition it watches clears.
    let mut totals: std::collections::BTreeMap<&'static str, i64> = transition::ALL
        .iter()
        .map(|status| (status_str(*status), 0))
        .collect();
    let mut oldest_unterminated = 0_i64;

    for row in rows {
        let status: String = row.try_get("status").map_err(PersistenceError::Query)?;
        let total: i64 = row.try_get("total").map_err(PersistenceError::Query)?;
        let oldest: i64 = row.try_get("oldest").map_err(PersistenceError::Query)?;
        let Some(known) = status_from_str(&status) else {
            // A status outside the closed set means the CHECK constraint was dropped. It is not
            // published, because an unbounded label is how a metric backend is taken down.
            tracing::warn!(%status, "an operation carries a status outside the known set");
            continue;
        };
        totals.insert(status_str(known), total);
        if !transition::is_terminal(known) {
            oldest_unterminated = oldest_unterminated.max(oldest);
        }
    }

    for (status, total) in totals {
        metrics::gauge!(
            platform_telemetry::metrics::PLATFORM_OPERATIONS,
            "status" => status,
        )
        .set(total as f64);
    }
    metrics::gauge!(
        platform_telemetry::metrics::PLATFORM_OPERATIONS_OLDEST_UNTERMINATED_AGE_SECONDS
    )
    .set(oldest_unterminated as f64);

    Ok(())
}

/// Attach a typed result reference.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the insert fails.
pub async fn record_result<'e, E>(
    executor: E,
    operation_id: Uuid,
    result: &OperationResultRef,
    now: jiff::Timestamp,
) -> Result<(), OperationError>
where
    E: PgExecutor<'e>,
{
    let payload = serde_json::to_value(result)
        .map_err(|error| OperationError::ContractViolation(error.to_string()))?;
    sqlx::query(
        "insert into operations.operation_results
             (result_id, operation_id, result_kind, target, payload, recorded_at)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(operation_id)
    .bind(String::from(result.result_kind.clone()))
    .bind(String::from(result.target.clone()))
    .bind(payload)
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(())
}

/// Attach a complete safe error and synchronize operation retryability.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the insert fails, including when the message is longer than
/// the schema allows or contains a newline — both of which are how a stack trace arrives.
pub async fn record_error(
    transaction: &mut sqlx::PgTransaction<'_>,
    operation_id: Uuid,
    error: &ErrorEnvelope,
    now: jiff::Timestamp,
) -> Result<(), OperationError> {
    sqlx::query("update operations.operations set retryable = $2 where operation_id = $1")
        .bind(operation_id)
        .bind(error.retryable)
        .execute(&mut **transaction)
        .await
        .map_err(PersistenceError::Query)?;
    let payload = serde_json::to_value(error)
        .map_err(|error| OperationError::ContractViolation(error.to_string()))?;
    insert_diagnostic(transaction, operation_id, "error", &payload, now).await
}

/// Attach a complete safe warning.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the insert fails.
pub async fn record_warning(
    transaction: &mut sqlx::PgTransaction<'_>,
    operation_id: Uuid,
    warning: &WarningEnvelope,
    now: jiff::Timestamp,
) -> Result<(), OperationError> {
    let payload = serde_json::to_value(warning)
        .map_err(|error| OperationError::ContractViolation(error.to_string()))?;
    insert_diagnostic(transaction, operation_id, "warning", &payload, now).await
}

async fn insert_diagnostic(
    transaction: &mut sqlx::PgTransaction<'_>,
    operation_id: Uuid,
    severity: &str,
    payload: &serde_json::Value,
    now: jiff::Timestamp,
) -> Result<(), OperationError> {
    sqlx::query(
        "insert into operations.operation_errors
             (error_id, operation_id, severity, code, message, retryable, payload, recorded_at)
         values ($1, $2, $3, $4 ->> 'code', $4 ->> 'message',
                 coalesce(($4 ->> 'retryable')::boolean, false), $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(operation_id)
    .bind(severity)
    .bind(payload)
    .bind(to_offset(now))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(())
}

/// Project a stored operation onto the public contract snapshot.
///
/// Every value is parsed through the contracts constructor rather than assembled from strings, so a
/// stored value that does not satisfy the contract is a typed failure here and never reaches a
/// client. The finished snapshot is then run through `OperationSnapshot::validate`, which checks the
/// cross-field invariants the schema cannot express — a `failed` operation with no error, or a
/// `succeeded` one that carries one.
///
/// # Errors
///
/// [`OperationError::NotFound`], [`OperationError::ContractViolation`] if a stored value or the
/// assembled snapshot violates the contract, [`OperationError::Persistence`] if a statement fails.
pub async fn snapshot<'e, E>(
    executor: E,
    operation_id: Uuid,
) -> Result<OperationSnapshot, OperationError>
where
    E: PgExecutor<'e> + Copy,
{
    let operation = find(executor, operation_id)
        .await?
        .ok_or(OperationError::NotFound)?;

    let violation =
        |what: &str, detail: String| OperationError::ContractViolation(format!("{what}: {detail}"));

    let kind = OperationKind::parse(&operation.kind)
        .map_err(|error| violation("kind", error.to_string()))?;
    let correlation_id = EntityRef::parse(&operation.correlation_id)
        .map_err(|error| violation("correlation_id", error.to_string()))?;
    let stage = operation
        .stage
        .as_deref()
        .map(OperationStage::parse)
        .transpose()
        .map_err(|error| violation("stage", error.to_string()))?;
    let progress_percent = operation
        .progress_percent
        .map(|value| {
            u8::try_from(value)
                .map_err(|error| violation("progress_percent", error.to_string()))
                .and_then(|value| {
                    ProgressPercent::new(value)
                        .map_err(|error| violation("progress_percent", error.to_string()))
                })
        })
        .transpose()?;

    let result_rows = sqlx::query(
        "select payload from operations.operation_results
          where operation_id = $1 order by recorded_at, result_id",
    )
    .bind(operation_id)
    .fetch_all(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let mut results = Vec::with_capacity(result_rows.len());
    for row in result_rows {
        let payload: serde_json::Value = row.try_get("payload").map_err(PersistenceError::Query)?;
        results.push(
            serde_json::from_value(payload)
                .map_err(|error| violation("result", error.to_string()))?,
        );
    }

    let (errors, warnings) = load_diagnostics(executor, operation_id).await?;

    let snapshot = OperationSnapshot {
        // The uuid newtypes construct from canonical text; `Uuid`'s `Display` is exactly that
        // form, so this parse is total for any value the database can hold.
        operation_id: OperationId::parse(&operation.operation_id.to_string())
            .map_err(|error| violation("operation_id", error.to_string()))?,
        kind,
        status: operation.status,
        stage,
        progress_percent,
        results,
        errors,
        warnings,
        retryable: operation.retryable,
        correlation_id,
        tenant_id: Some(TenantRef::of_user(
            UserId::parse(&operation.owner_user_id.to_string())
                .map_err(|error| violation("owner_user_id", error.to_string()))?,
        )),
        accepted_at: WireTimestamp::from_jiff(operation.accepted_at),
        status_changed_at: WireTimestamp::from_jiff(operation.status_changed_at),
        terminated_at: operation.terminated_at.map(WireTimestamp::from_jiff),
        extensions: Extensions::default(),
    };

    snapshot
        .validate()
        .map_err(|error| OperationError::ContractViolation(error.to_string()))?;

    Ok(snapshot)
}

/// Read the stored errors and warnings of one operation, already parsed into contract types.
///
/// Split out of [`snapshot`] so that function stays inside the workspace's length lint, and along a
/// boundary that means something: this is the only place a stored diagnostic becomes a wire value.
async fn load_diagnostics<'e, E>(
    executor: E,
    operation_id: Uuid,
) -> Result<(Vec<ErrorEnvelope>, Vec<WarningEnvelope>), OperationError>
where
    E: PgExecutor<'e>,
{
    let violation =
        |what: &str, detail: String| OperationError::ContractViolation(format!("{what}: {detail}"));

    let rows = sqlx::query(
        "select severity, payload from operations.operation_errors
          where operation_id = $1 order by recorded_at, error_id",
    )
    .bind(operation_id)
    .fetch_all(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for row in rows {
        let severity: String = row.try_get("severity").map_err(PersistenceError::Query)?;
        let payload: serde_json::Value = row.try_get("payload").map_err(PersistenceError::Query)?;

        if severity == "warning" {
            warnings.push(
                serde_json::from_value(payload)
                    .map_err(|error| violation("warning", error.to_string()))?,
            );
        } else {
            errors.push(
                serde_json::from_value(payload)
                    .map_err(|error| violation("error", error.to_string()))?,
            );
        }
    }

    Ok((errors, warnings))
}

/// One recorded progress entry, as a client sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProgressEntry {
    /// The entry's identity. `UUIDv7`, so ordering by it is ordering by time, and a client can resume
    /// from the last one it saw.
    pub progress_id: Uuid,
    /// The status this entry recorded.
    pub status: OperationStatus,
    /// The display phase, when the producer gave one.
    pub stage: Option<String>,
    /// Bounded progress, when the producer gave one.
    pub progress_percent: Option<u8>,
    /// A user-safe message.
    pub message: Option<String>,
    /// When it was observed.
    pub observed_at: WireTimestamp,
}

/// Read progress entries recorded after `after`, oldest first.
///
/// `after` is a progress id rather than a timestamp: two entries can share an instant, and a client
/// resuming from a timestamp would either repeat one or miss one. `UUIDv7` orders by time, so this is
/// both a cursor and a chronology.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the statement fails.
pub async fn progress_since<'e, E>(
    executor: E,
    operation_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<ProgressEntry>, OperationError>
where
    E: PgExecutor<'e>,
{
    let rows = sqlx::query(
        "select progress_id, status, stage, progress_percent, message, observed_at
           from operations.operation_progress
          where operation_id = $1 and ($2::uuid is null or progress_id > $2)
          order by progress_id
          limit $3",
    )
    .bind(operation_id)
    .bind(after)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(PersistenceError::Query)?;

    rows.into_iter()
        .map(|row| {
            let status: String = row.try_get("status").map_err(PersistenceError::Query)?;
            let percent: Option<i16> = row
                .try_get("progress_percent")
                .map_err(PersistenceError::Query)?;
            Ok(ProgressEntry {
                progress_id: row
                    .try_get("progress_id")
                    .map_err(PersistenceError::Query)?,
                status: status_from_str(&status).ok_or_else(|| {
                    OperationError::ContractViolation(format!("unknown stored status {status}"))
                })?,
                stage: row.try_get("stage").map_err(PersistenceError::Query)?,
                progress_percent: percent.and_then(|value| u8::try_from(value).ok()),
                message: row.try_get("message").map_err(PersistenceError::Query)?,
                observed_at: WireTimestamp::from_jiff(from_offset(
                    row.try_get("observed_at")
                        .map_err(PersistenceError::Query)?,
                )),
            })
        })
        .collect()
}
