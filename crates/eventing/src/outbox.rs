//! The transactional outbox.
//!
//! Enqueuing takes an executor so it joins the caller's transaction. That is the entire point: the
//! state change and the message that announces it commit together or not at all.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{EventingError, Subject, from_offset, to_offset};

/// How long a claim is held before the row returns to the queue.
///
/// A lease rather than a flag: a publisher that is killed mid-batch cannot release its rows, and
/// without an expiry those messages would wait for a human.
const CLAIM_LEASE_SECONDS: i64 = 30;

/// The ceiling on backoff. `ARCHITECTURE.md` S8.2 requires retry with BOUNDED backoff; unbounded
/// doubling reaches intervals where a recovered broker goes unnoticed for hours.
const MAX_BACKOFF_SECONDS: i64 = 300;

/// How many attempts a message gets before it is dead-lettered.
const MAX_ATTEMPTS: i32 = 12;

/// A row claimed for publication.
#[derive(Debug, Clone)]
pub struct ClaimedMessage {
    /// The row.
    pub outbox_id: Uuid,
    /// The envelope identity, which is also the consumer's deduplication key.
    pub message_id: Uuid,
    /// Where it goes.
    pub subject: Subject,
    /// The serialized envelope.
    pub payload: serde_json::Value,
    /// How many times publication has already been attempted.
    pub attempts: i32,
}

/// What the backlog looks like. `AGENTS.md` requires outbox lag as a telemetry signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxStats {
    /// Rows neither published nor dead-lettered.
    pub pending: i64,
    /// Rows that exhausted their attempts.
    pub dead_lettered: i64,
    /// Age in seconds of the oldest pending row, or zero when there is none. This is the lag an
    /// operator alarms on: a count alone cannot distinguish a busy queue from a stuck one.
    pub oldest_pending_age_seconds: i64,
}

/// The outbox.
#[derive(Debug, Clone, Copy)]
pub struct Outbox;

impl Outbox {
    /// Enqueue a message inside the caller's transaction.
    ///
    /// Idempotent on `message_id`: re-running the same transaction body cannot produce two
    /// messages, which is what makes a retried request safe.
    ///
    /// Returns `false` when the message was already enqueued.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn enqueue<'e, E>(
        executor: E,
        message_id: Uuid,
        subject: &Subject,
        payload: &serde_json::Value,
        operation_id: Option<Uuid>,
        now: jiff::Timestamp,
    ) -> Result<bool, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let result = sqlx::query(
            "insert into operations.outbox
                 (outbox_id, message_id, subject, payload, operation_id, enqueued_at, next_attempt_at)
             values ($1, $2, $3, $4, $5, $6, $6)
             on conflict (message_id) do nothing",
        )
        .bind(Uuid::now_v7())
        .bind(message_id)
        .bind(subject.as_str())
        .bind(payload)
        .bind(operation_id)
        .bind(to_offset(now))
        .execute(executor)
        .await
        .map_err(PersistenceError::Query)?;

        Ok(result.rows_affected() == 1)
    }

    /// Claim up to `limit` due messages for this publisher.
    ///
    /// `for update skip locked` is what lets several publishers run without coordinating: each takes
    /// rows the others are not holding, and none blocks.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if a statement fails, [`EventingError::InvalidSubject`] if a
    /// stored subject is not in the grammar — which would mean the CHECK constraint was bypassed.
    pub async fn claim<'e, E>(
        executor: E,
        claimed_by: &str,
        limit: i64,
        now: jiff::Timestamp,
    ) -> Result<Vec<ClaimedMessage>, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let lease_until = now + jiff::SignedDuration::from_secs(CLAIM_LEASE_SECONDS);
        let rows = sqlx::query(
            "with due as (
                 select outbox_id from operations.outbox
                  where published_at is null
                    and dead_lettered_at is null
                    and next_attempt_at <= $3
                    and (claimed_until is null or claimed_until <= $3)
                  order by next_attempt_at, enqueued_at
                  limit $2
                  for update skip locked
             )
             update operations.outbox o
                set claimed_until = $4, claimed_by = $1
               from due
              where o.outbox_id = due.outbox_id
              returning o.outbox_id, o.message_id, o.subject, o.payload, o.attempts",
        )
        .bind(claimed_by)
        .bind(limit)
        .bind(to_offset(now))
        .bind(to_offset(lease_until))
        .fetch_all(executor)
        .await
        .map_err(PersistenceError::Query)?;

        rows.into_iter()
            .map(|row| {
                let subject: String = row.try_get("subject").map_err(PersistenceError::Query)?;
                Ok(ClaimedMessage {
                    outbox_id: row.try_get("outbox_id").map_err(PersistenceError::Query)?,
                    message_id: row.try_get("message_id").map_err(PersistenceError::Query)?,
                    subject: Subject::parse(&subject)?,
                    payload: row.try_get("payload").map_err(PersistenceError::Query)?,
                    attempts: row.try_get("attempts").map_err(PersistenceError::Query)?,
                })
            })
            .collect()
    }

    /// Mark a message delivered.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn mark_published<'e, E>(
        executor: E,
        outbox_id: Uuid,
        now: jiff::Timestamp,
    ) -> Result<(), EventingError>
    where
        E: PgExecutor<'e>,
    {
        sqlx::query(
            "update operations.outbox
                set published_at = $2, claimed_until = null, claimed_by = null, last_error = null
              where outbox_id = $1",
        )
        .bind(outbox_id)
        .bind(to_offset(now))
        .execute(executor)
        .await
        .map_err(PersistenceError::Query)?;
        Ok(())
    }

    /// Record a failed attempt: back the message off, or dead-letter it once exhausted.
    ///
    /// Returns `true` when the message was dead-lettered.
    ///
    /// The error text is truncated to what the column allows and stripped of line breaks, because a
    /// client library's error can be a multi-line chain and the column is read by an operator.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn mark_failed<'e, E>(
        executor: E,
        outbox_id: Uuid,
        error: &str,
        now: jiff::Timestamp,
    ) -> Result<bool, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let safe = safe_error(error);
        let row = sqlx::query(
            "update operations.outbox
                set attempts = attempts + 1,
                    last_error = $2,
                    claimed_until = null,
                    claimed_by = null,
                    next_attempt_at = $3 + make_interval(secs => least($4, power(2, least(attempts, 20))::double precision)),
                    dead_lettered_at = case when attempts + 1 >= $5 then $3 end
              where outbox_id = $1
              returning dead_lettered_at is not null as dead_lettered",
        )
        .bind(outbox_id)
        .bind(&safe)
        .bind(to_offset(now))
        .bind(f64::from(u32::try_from(MAX_BACKOFF_SECONDS).unwrap_or(300)))
        .bind(MAX_ATTEMPTS)
        .fetch_optional(executor)
        .await
        .map_err(PersistenceError::Query)?;

        row.map_or(Ok(false), |row| {
            row.try_get::<bool, _>("dead_lettered")
                .map_err(|error| EventingError::Persistence(PersistenceError::Query(error)))
        })
    }

    /// The backlog, for the outbox-lag signal `AGENTS.md` requires.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn stats<'e, E>(
        executor: E,
        now: jiff::Timestamp,
    ) -> Result<OutboxStats, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let row = sqlx::query(
            "select
                 count(*) filter (where published_at is null and dead_lettered_at is null) as pending,
                 count(*) filter (where dead_lettered_at is not null) as dead_lettered,
                 coalesce(
                     extract(epoch from $1 - min(enqueued_at)
                         filter (where published_at is null and dead_lettered_at is null)),
                     0)::bigint as oldest
               from operations.outbox",
        )
        .bind(to_offset(now))
        .fetch_one(executor)
        .await
        .map_err(PersistenceError::Query)?;

        Ok(OutboxStats {
            pending: row.try_get("pending").map_err(PersistenceError::Query)?,
            dead_lettered: row
                .try_get("dead_lettered")
                .map_err(PersistenceError::Query)?,
            oldest_pending_age_seconds: row.try_get("oldest").map_err(PersistenceError::Query)?,
        })
    }

    /// Delete published rows older than `before`, at most `limit` of them.
    ///
    /// Published only. A dead-lettered row is evidence of work a client was told had been accepted
    /// and that nobody delivered, so it is kept until a person resolves it — a retention window that
    /// quietly disposed of those would make `platform_outbox_dead_lettered` a gauge that falls on
    /// its own.
    ///
    /// Bounded for the same reason [`crate::Inbox::collect_processed`] is.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn collect_published<'e, E>(
        executor: E,
        before: jiff::Timestamp,
        limit: i64,
    ) -> Result<u64, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let done = sqlx::query(
            "delete from operations.outbox
              where outbox_id in (
                  select outbox_id from operations.outbox
                   where published_at is not null and published_at < $1
                   limit $2
              )",
        )
        .bind(to_offset(before))
        .bind(limit)
        .execute(executor)
        .await
        .map_err(PersistenceError::Query)?;
        Ok(done.rows_affected())
    }

    /// When a claimed message's lease expires, for a publisher deciding whether to keep going.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn claim_expiry<'e, E>(
        executor: E,
        outbox_id: Uuid,
    ) -> Result<Option<jiff::Timestamp>, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let row = sqlx::query("select claimed_until from operations.outbox where outbox_id = $1")
            .bind(outbox_id)
            .fetch_optional(executor)
            .await
            .map_err(PersistenceError::Query)?;

        Ok(row
            .and_then(|row| {
                row.try_get::<Option<time::OffsetDateTime>, _>("claimed_until")
                    .ok()
            })
            .flatten()
            .map(from_offset))
    }
}

/// Fit an arbitrary error chain into the column's rule: one line, at most 200 characters.
fn safe_error(error: &str) -> String {
    let single_line: String = error
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = single_line.trim();
    if trimmed.chars().count() <= 200 {
        return trimmed.to_owned();
    }
    trimmed.chars().take(197).collect::<String>() + "..."
}

#[cfg(test)]
mod tests {
    use super::safe_error;

    #[test]
    fn an_error_is_flattened_and_bounded_to_fit_the_column() {
        let multi = "connection refused\n  caused by: broken pipe\r\n  at some::place";
        let safe = safe_error(multi);
        assert!(!safe.contains('\n') && !safe.contains('\r'), "{safe}");
        assert!(safe.chars().count() <= 200);

        let long = "x".repeat(500);
        let safe = safe_error(&long);
        assert_eq!(safe.chars().count(), 200);
        assert!(safe.ends_with("..."));
    }
}
