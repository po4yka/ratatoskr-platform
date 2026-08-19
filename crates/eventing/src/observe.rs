//! The gauges an operator reads when the queue looks wrong.
//!
//! Sampled on a timer rather than published from the write path. Every number here is an aggregate
//! over a whole table, so computing it on each enqueue would put a full scan on the request path to
//! keep a gauge fresh between scrapes that are fifteen seconds apart.
//!
//! Separated from [`crate::outbox`] because the split is real: `Outbox::stats` answers a question,
//! and this decides that the answer is worth publishing under a name a dashboard depends on.

use sqlx::PgPool;

use platform_telemetry::metrics::{
    PLATFORM_INBOX_UNPROCESSED, PLATFORM_OUTBOX_DEAD_LETTERED,
    PLATFORM_OUTBOX_OLDEST_PENDING_AGE_SECONDS, PLATFORM_OUTBOX_PENDING,
};

use crate::{EventingError, Inbox, Outbox};

/// Sample the outbox and the inbox and publish their gauges.
///
/// One call per tick from the process that owns the publisher. It runs two aggregates and nothing
/// else, so it is safe to call while the pump is working: neither statement takes a lock the pump
/// would wait on.
///
/// # Errors
///
/// [`EventingError::Persistence`] if either aggregate fails. The caller logs it and tries again on
/// the next tick — a failed sample is a missing point in a series, not a reason to stop.
#[expect(
    clippy::cast_precision_loss,
    reason = "queue depths and an age in seconds, exported as f64 gauges; both are exact well past \
              any value that is not already an incident"
)]
pub async fn sample(pool: &PgPool, now: jiff::Timestamp) -> Result<(), EventingError> {
    let stats = Outbox::stats(pool, now).await?;
    metrics::gauge!(PLATFORM_OUTBOX_PENDING).set(stats.pending as f64);
    metrics::gauge!(PLATFORM_OUTBOX_DEAD_LETTERED).set(stats.dead_lettered as f64);
    metrics::gauge!(PLATFORM_OUTBOX_OLDEST_PENDING_AGE_SECONDS)
        .set(stats.oldest_pending_age_seconds as f64);

    let unprocessed = Inbox::unprocessed(pool).await?;
    metrics::gauge!(PLATFORM_INBOX_UNPROCESSED).set(unprocessed as f64);

    Ok(())
}
