//! The inbox: the processed-event record that makes at-least-once delivery safe to consume.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{EventingError, Subject, to_offset};

/// What happened when a message arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reception {
    /// First delivery. The caller must handle it and then call [`Inbox::finish`].
    First,
    /// Already recorded. The caller must not handle it again, and this is not an error: at-least-once
    /// delivery makes redelivery ordinary traffic.
    Duplicate,
}

/// What handling a message produced.
///
/// The vocabulary is the operation transition table's, so an operator reading the inbox and an
/// operator reading operation history see the same words for the same thing (ADR-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The message advanced state.
    Applied,
    /// The message repeated state that was already current.
    Duplicate,
    /// The message carried an older state than the current one.
    Stale,
    /// The message was well-formed but refused, e.g. two conflicting terminal outcomes.
    Rejected,
}

impl Outcome {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Duplicate => "duplicate",
            Self::Stale => "stale",
            Self::Rejected => "rejected",
        }
    }
}

/// The inbox.
#[derive(Debug, Clone, Copy)]
pub struct Inbox;

impl Inbox {
    /// Claim a message for handling, or discover that it has already been seen.
    ///
    /// The deduplication IS the insert. `on conflict do nothing` and the primary key decide in one
    /// statement, so two workers receiving the same redelivery cannot both conclude they are first —
    /// which a read-then-write would allow.
    ///
    /// Takes an executor so it joins the caller's transaction: the inbox record and the state change
    /// it authorises must commit together, or a crash between them replays work that was already
    /// applied.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn begin<'e, E>(
        executor: E,
        message_id: Uuid,
        subject: &Subject,
        producer: &str,
        now: jiff::Timestamp,
    ) -> Result<Reception, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let result = sqlx::query(
            "insert into operations.inbox (message_id, subject, producer, received_at)
             values ($1, $2, $3, $4)
             on conflict (message_id) do nothing",
        )
        .bind(message_id)
        .bind(subject.as_str())
        .bind(producer)
        .bind(to_offset(now))
        .execute(executor)
        .await
        .map_err(PersistenceError::Query)?;

        Ok(if result.rows_affected() == 1 {
            Reception::First
        } else {
            Reception::Duplicate
        })
    }

    /// Record what handling produced.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn finish<'e, E>(
        executor: E,
        message_id: Uuid,
        outcome: Outcome,
        now: jiff::Timestamp,
    ) -> Result<(), EventingError>
    where
        E: PgExecutor<'e>,
    {
        sqlx::query(
            "update operations.inbox set processed_at = $2, outcome = $3 where message_id = $1",
        )
        .bind(message_id)
        .bind(to_offset(now))
        .bind(outcome.as_str())
        .execute(executor)
        .await
        .map_err(PersistenceError::Query)?;
        Ok(())
    }

    /// How many messages were received but never finished.
    ///
    /// A non-zero steady state means handlers are crashing after claiming, which is the failure the
    /// inbox cannot fix by itself and an operator must see.
    ///
    /// # Errors
    ///
    /// [`EventingError::Persistence`] if the statement fails.
    pub async fn unprocessed<'e, E>(executor: E) -> Result<i64, EventingError>
    where
        E: PgExecutor<'e>,
    {
        let row = sqlx::query(
            "select count(*) as count from operations.inbox where processed_at is null",
        )
        .fetch_one(executor)
        .await
        .map_err(PersistenceError::Query)?;
        row.try_get::<i64, _>("count")
            .map_err(|error| EventingError::Persistence(PersistenceError::Query(error)))
    }
}
