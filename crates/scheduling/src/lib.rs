//! Periodic command publication.
//!
//! `ARCHITECTURE.md` S10 gives Scheduler six steps and no seventh:
//!
//! ```text
//! schedule definition -> due trigger -> acquire schedule lease
//! -> create deterministic occurrence ID -> publish command through outbox -> record outcome
//! ```
//!
//! Every one of them is here, and nothing else is. Scheduler "does not import domain repositories
//! or decide domain behavior", so this crate knows what a schedule row says and knows nothing about
//! what the command it publishes will cause.
//!
//! # It publishes into the outbox, not onto the bus
//!
//! The command is written to `operations.outbox` in the same transaction as the occurrence and the
//! operation, and `ratatoskr-edge` moves it to the broker. That is a decided deployment fact rather
//! than an omission (ADR-0013): one pump on one host, so a scheduler that cannot reach the broker
//! still records the occurrence durably, and a broker outage becomes a backlog with an operator
//! signal instead of a lost occurrence. This crate therefore has no NATS dependency at all.

use jiff::{SignedDuration, Timestamp};
use platform_eventing::{Command, MessageClass, Outbox, Subject};
use platform_persistence::PersistenceError;
use sqlx::{PgPool, Postgres, Row as _, Transaction};
use uuid::Uuid;

pub mod plan;

pub use plan::{Advance, CatchUp, occurrence_id};

/// Why a pass could not do what it was asked.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SchedulingError {
    /// The database refused or failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// The outbox refused the command.
    #[error(transparent)]
    Eventing(#[from] platform_eventing::EventingError),

    /// The operation record could not be created.
    #[error(transparent)]
    Operation(#[from] platform_operations::OperationError),

    /// A stored row does not satisfy a constraint the schema is supposed to guarantee, so the
    /// constraint has been dropped. Publishing anyway would act on a value nobody validated.
    #[error("schedule {schedule_id} holds an unusable {column}")]
    UnusableRow {
        /// Which row.
        schedule_id: Uuid,
        /// Which column.
        column: &'static str,
    },
}

/// What one pass did. Every field is a signal `ARCHITECTURE.md` S16 asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchedulerReport {
    /// Schedules that were due when the pass began.
    pub due: usize,
    /// Occurrences published into the outbox.
    pub published: usize,
    /// Occurrences that already existed and were therefore not published a second time. The
    /// duplicate-suppression signal.
    pub suppressed: usize,
    /// Grid points passed over without being published, by policy.
    pub skipped: u64,
    /// Schedules whose transaction failed. Their rows are untouched and the next pass retries them.
    pub failed: usize,
}

/// One schedule, as stored.
#[derive(Debug, Clone)]
struct Schedule {
    /// Named `id` rather than `schedule_id`: inside a `Schedule` the prefix says nothing, and the
    /// column it comes from is spelled once, in the `select` above the struct.
    id: Uuid,
    name: String,
    owner_user_id: Uuid,
    command_type: String,
    operation_kind: String,
    payload: serde_json::Value,
    interval: SignedDuration,
    next_due_at: Timestamp,
    catch_up: CatchUp,
}

/// What handling one schedule produced.
#[derive(Debug, Clone)]
struct Handled {
    name: String,
    published: bool,
    drift_seconds: i64,
    skipped: u32,
}

/// Publish every occurrence that is due, up to `limit` schedules.
///
/// One transaction per schedule rather than one for the pass: a schedule whose command type has
/// become unpublishable must not roll back the occurrences of the others, exactly as one poison
/// message does not stall an outbox pass.
///
/// # Errors
///
/// [`SchedulingError::Persistence`] if the due-schedule query itself fails. A failure inside one
/// schedule's transaction is counted in the report, not returned.
pub async fn run_once(
    pool: &PgPool,
    limit: i64,
    now: Timestamp,
) -> Result<SchedulerReport, SchedulingError> {
    let due = due_schedules(pool, limit, now).await?;
    let mut report = SchedulerReport {
        due: due.len(),
        ..SchedulerReport::default()
    };

    for schedule_id in due {
        match handle(pool, schedule_id, now).await {
            Ok(Some(handled)) => record(&mut report, &handled),
            // The row was taken by another holder of the lease, disabled, or rescheduled between
            // the two statements. Nothing to do and nothing wrong.
            Ok(None) => {}
            Err(error) => {
                report.failed += 1;
                tracing::warn!(%schedule_id, %error, "a schedule could not be published");
            }
        }
    }

    Ok(report)
}

/// Fold one handled schedule into the report and into the exported signals.
fn record(report: &mut SchedulerReport, handled: &Handled) {
    report.skipped += u64::from(handled.skipped);
    let outcome = if handled.published {
        report.published += 1;
        "published"
    } else {
        report.suppressed += 1;
        "suppressed"
    };

    // A gauge rather than a histogram: a schedule publishes once per interval, so "how late was
    // the last one" is the whole question, and a histogram of one sample per minute over shared
    // latency buckets that stop at ten seconds would answer it worse.
    #[expect(
        clippy::cast_precision_loss,
        reason = "drift is a duration in seconds exported as a gauge; f64 is exact to 2^53 seconds"
    )]
    metrics::gauge!(
        platform_telemetry::metrics::PLATFORM_SCHEDULER_DRIFT_SECONDS,
        "schedule" => handled.name.clone(),
    )
    .set(handled.drift_seconds as f64);

    metrics::counter!(
        platform_telemetry::metrics::PLATFORM_SCHEDULER_OCCURRENCES_TOTAL,
        "schedule" => handled.name.clone(),
        "outcome" => outcome,
    )
    .increment(1);

    if handled.skipped > 0 {
        metrics::counter!(
            platform_telemetry::metrics::PLATFORM_SCHEDULER_OCCURRENCES_TOTAL,
            "schedule" => handled.name.clone(),
            "outcome" => "skipped",
        )
        .increment(u64::from(handled.skipped));
    }
}

/// Which schedules are due. Read without a lock: the lock is taken per schedule, in [`handle`].
async fn due_schedules(
    pool: &PgPool,
    limit: i64,
    now: Timestamp,
) -> Result<Vec<Uuid>, SchedulingError> {
    let rows = sqlx::query(
        "select schedule_id from operations.schedules
          where enabled and next_due_at <= $1
          order by next_due_at
          limit $2",
    )
    .bind(to_offset(now))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(PersistenceError::Query)?;

    rows.into_iter()
        .map(|row| {
            row.try_get("schedule_id")
                .map_err(|error| SchedulingError::Persistence(PersistenceError::Query(error)))
        })
        .collect()
}

/// Take the lease on one schedule, publish its occurrence if it has not been published, and move
/// the schedule forward. All of it in one transaction, so an interrupted process leaves either a
/// complete occurrence or none.
async fn handle(
    pool: &PgPool,
    schedule_id: Uuid,
    now: Timestamp,
) -> Result<Option<Handled>, SchedulingError> {
    let mut transaction = pool.begin().await.map_err(PersistenceError::Query)?;

    let Some(schedule) = claim(&mut transaction, schedule_id, now).await? else {
        return Ok(None);
    };

    let due_at = schedule.next_due_at;
    let occurrence_id = plan::occurrence_id(schedule.id, due_at);
    let drift_seconds = now.duration_since(due_at).as_secs().max(0);

    // Read rather than an `on conflict` on the insert, because the answer decides whether an
    // operation is created at all — and because it is race-free here: the schedule row is locked,
    // and only the holder of that lock writes occurrences for it. The primary key stays as the
    // guarantee that does not depend on this being true.
    let published = if recorded(&mut transaction, occurrence_id).await? {
        false
    } else {
        publish(
            &mut transaction,
            &schedule,
            occurrence_id,
            drift_seconds,
            now,
        )
        .await?;
        true
    };

    let advance = plan::advance(due_at, schedule.interval, now, schedule.catch_up);
    reschedule(&mut transaction, schedule.id, &advance, published, now).await?;
    transaction
        .commit()
        .await
        .map_err(PersistenceError::Query)?;

    Ok(Some(Handled {
        name: schedule.name,
        published,
        drift_seconds,
        skipped: advance.skipped,
    }))
}

/// Lock the schedule and read it back, or report that somebody else has it.
///
/// `for update skip locked` is the lease of S10. ADR-0010 keeps it with one process per role
/// because a restart overlapping a drain is two processes for a few seconds, which is all it takes
/// to publish one occurrence twice.
async fn claim(
    transaction: &mut Transaction<'_, Postgres>,
    schedule_id: Uuid,
    now: Timestamp,
) -> Result<Option<Schedule>, SchedulingError> {
    let row = sqlx::query(
        "select schedule_id, name, owner_user_id, command_type, operation_kind, payload,
                interval_seconds, next_due_at, catch_up
           from operations.schedules
          where schedule_id = $1 and enabled and next_due_at <= $2
            for update skip locked",
    )
    .bind(schedule_id)
    .bind(to_offset(now))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else { return Ok(None) };

    let catch_up: String = row.try_get("catch_up").map_err(PersistenceError::Query)?;
    let Some(catch_up) = CatchUp::parse(&catch_up) else {
        return Err(SchedulingError::UnusableRow {
            schedule_id,
            column: "catch_up",
        });
    };
    let interval_seconds: i32 = row
        .try_get("interval_seconds")
        .map_err(PersistenceError::Query)?;
    let next_due_at: time::OffsetDateTime = row
        .try_get("next_due_at")
        .map_err(PersistenceError::Query)?;

    Ok(Some(Schedule {
        id: schedule_id,
        name: row.try_get("name").map_err(PersistenceError::Query)?,
        owner_user_id: row
            .try_get("owner_user_id")
            .map_err(PersistenceError::Query)?,
        command_type: row
            .try_get("command_type")
            .map_err(PersistenceError::Query)?,
        operation_kind: row
            .try_get("operation_kind")
            .map_err(PersistenceError::Query)?,
        payload: row.try_get("payload").map_err(PersistenceError::Query)?,
        interval: SignedDuration::from_secs(i64::from(interval_seconds)),
        next_due_at: from_offset(next_due_at),
        catch_up,
    }))
}

/// Whether this occurrence has already been published.
async fn recorded(
    transaction: &mut Transaction<'_, Postgres>,
    occurrence_id: Uuid,
) -> Result<bool, SchedulingError> {
    let row = sqlx::query(
        "select 1 as present from operations.schedule_occurrences where occurrence_id = $1",
    )
    .bind(occurrence_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(row.is_some())
}

/// Create the operation, record the occurrence, and enqueue the command — steps 4 to 6 of S10.
///
/// The occurrence identifier is used three times: as the primary key here, as the operation's
/// idempotency key, and as the outbox `message_id`. One identifier, three independent refusals of
/// a duplicate.
async fn publish(
    transaction: &mut Transaction<'_, Postgres>,
    schedule: &Schedule,
    occurrence_id: Uuid,
    drift_seconds: i64,
    now: Timestamp,
) -> Result<(), SchedulingError> {
    let subject = Subject::new(MessageClass::Command, &schedule.command_type)?;
    let correlation = platform_telemetry::correlation::mint_correlation().to_string();
    let idempotency_key = occurrence_id.to_string();

    let operation = platform_operations::accept(
        &mut **transaction,
        schedule.owner_user_id,
        &schedule.operation_kind,
        &correlation,
        Some(&idempotency_key),
        now,
    )
    .await?;

    sqlx::query(
        "insert into operations.schedule_occurrences
             (occurrence_id, schedule_id, due_at, published_at, drift_seconds, operation_id)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(occurrence_id)
    .bind(schedule.id)
    .bind(to_offset(schedule.next_due_at))
    .bind(to_offset(now))
    .bind(drift_seconds)
    .bind(operation.operation_id)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let envelope = Command {
        command_type: &schedule.command_type,
        operation_id: operation.operation_id,
        principal: schedule.owner_user_id,
        correlation_id: &correlation,
        idempotency_key: &idempotency_key,
        requested_at: now,
    }
    .envelope(schedule.payload.clone());

    Outbox::enqueue(
        &mut **transaction,
        occurrence_id,
        &subject,
        &envelope,
        Some(operation.operation_id),
        now,
    )
    .await?;

    Ok(())
}

/// Move the schedule to its next due time.
async fn reschedule(
    transaction: &mut Transaction<'_, Postgres>,
    schedule_id: Uuid,
    advance: &Advance,
    published: bool,
    now: Timestamp,
) -> Result<(), SchedulingError> {
    sqlx::query(
        "update operations.schedules
            set next_due_at = $2,
                updated_at = $3,
                last_published_at = case when $4 then $3 else last_published_at end
          where schedule_id = $1",
    )
    .bind(schedule_id)
    .bind(to_offset(advance.next_due_at))
    .bind(to_offset(now))
    .bind(published)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(())
}

/// Whether the migrations that own these tables have been applied.
///
/// `ratatoskr-scheduler` does not migrate: `ARCHITECTURE.md` S18 gives it its own least-privilege
/// database role, and a role that may create a table is not one. `ratatoskr-edge` owns the
/// migrations, so this is the check that turns "edge has never run here" into one sentence at
/// startup instead of a Postgres error on the first tick.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the catalogue cannot be read.
pub async fn schema_is_present(pool: &PgPool) -> Result<bool, PersistenceError> {
    let present: Option<String> =
        sqlx::query_scalar("select to_regclass('operations.schedules')::text")
            .fetch_one(pool)
            .await
            .map_err(PersistenceError::Query)?;
    Ok(present.is_some())
}

/// jiff on the wire, `time` in the driver. Through unix nanoseconds, which needs no calendar.
fn to_offset(value: Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

/// The reverse.
fn from_offset(value: time::OffsetDateTime) -> Timestamp {
    Timestamp::from_nanosecond(value.unix_timestamp_nanos()).unwrap_or(Timestamp::UNIX_EPOCH)
}
