//! The loop that moves claimed outbox rows onto the bus.
//!
//! One pass, not a daemon. The caller decides the cadence, which keeps this testable without a
//! clock and lets a service bind it to whatever scheduler it already has.

use sqlx::PgPool;

use crate::{EventingError, Outbox, Publisher};

/// What one pass did. Every field is a signal `AGENTS.md` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PumpReport {
    /// Rows claimed this pass.
    pub claimed: usize,
    /// Rows the bus acknowledged.
    pub published: usize,
    /// Rows whose publication failed and were backed off.
    pub failed: usize,
    /// Rows that exhausted their attempts and were dead-lettered this pass.
    pub dead_lettered: usize,
}

/// Claim up to `limit` due messages and publish them.
///
/// A failure to publish one message does not abandon the pass: the remaining claimed rows are still
/// attempted, because one poison message must not stall a queue. That is also why the result is a
/// report rather than an error — a partly successful pass is the normal case when a broker is
/// flapping, and collapsing it into `Err` would lose the successes.
///
/// # Errors
///
/// [`EventingError::Persistence`] if claiming itself fails. A publication failure is recorded
/// against the row, not returned.
pub async fn run_once<P>(
    pool: &PgPool,
    publisher: &P,
    claimed_by: &str,
    limit: i64,
    now: jiff::Timestamp,
) -> Result<PumpReport, EventingError>
where
    P: Publisher,
{
    let claimed = Outbox::claim(pool, claimed_by, limit, now).await?;
    let mut report = PumpReport {
        claimed: claimed.len(),
        ..PumpReport::default()
    };

    for message in claimed {
        let body = match serde_json::to_vec(&message.payload) {
            Ok(body) => body,
            Err(error) => {
                // A row whose payload cannot be serialized will never succeed, so it takes the
                // failure path and reaches the dead-letter queue on its own schedule rather than
                // being retried as if the broker were at fault.
                report.failed += 1;
                if Outbox::mark_failed(pool, message.outbox_id, &error.to_string(), now).await? {
                    report.dead_lettered += 1;
                }
                continue;
            }
        };

        match publisher
            .publish(&message.subject, &body, &message.message_id.to_string())
            .await
        {
            Ok(()) => {
                Outbox::mark_published(pool, message.outbox_id, now).await?;
                report.published += 1;
            }
            Err(error) => {
                report.failed += 1;
                if Outbox::mark_failed(pool, message.outbox_id, &error.to_string(), now).await? {
                    report.dead_lettered += 1;
                }
            }
        }
    }

    Ok(report)
}
