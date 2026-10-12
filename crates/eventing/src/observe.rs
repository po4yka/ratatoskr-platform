//! The gauges an operator reads when the queue looks wrong.
//!
//! Sampled on a timer rather than published from the write path. Every number here is an aggregate
//! over a whole table, so computing it on each enqueue would put a full scan on the request path to
//! keep a gauge fresh between scrapes that are fifteen seconds apart.
//!
//! Separated from [`crate::outbox`] because the split is real: `Outbox::stats` answers a question,
//! and this decides that the answer is worth publishing under a name a dashboard depends on.

use async_nats::jetstream;
use sqlx::PgPool;

use platform_telemetry::metrics::{
    PLATFORM_BUS_CONSUMER_ACK_PENDING, PLATFORM_BUS_CONSUMER_OLDEST_UNACKED_AGE_SECONDS,
    PLATFORM_BUS_CONSUMER_PENDING, PLATFORM_BUS_CONSUMER_REDELIVERED, PLATFORM_BUS_STREAM_BYTES,
    PLATFORM_BUS_STREAM_MAX_BYTES, PLATFORM_BUS_STREAM_MESSAGES,
    PLATFORM_BUS_STREAM_OLDEST_MESSAGE_AGE_SECONDS, PLATFORM_INBOX_UNPROCESSED,
    PLATFORM_OUTBOX_DEAD_LETTERED, PLATFORM_OUTBOX_OLDEST_PENDING_AGE_SECONDS,
    PLATFORM_OUTBOX_PENDING,
};

use crate::{COMMAND_STREAM, EVENT_STREAM, EventingError, FixedConsumerSpec, Inbox, Outbox};

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

/// The age of `instant` at `now`, in seconds, floored at zero: a stream timestamp from the future
/// (a broker clock ahead of this one) is not a negative lag.
fn age_seconds(now: jiff::Timestamp, instant: time::OffsetDateTime) -> f64 {
    let stamped = jiff::Timestamp::from_nanosecond(instant.unix_timestamp_nanos())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    #[expect(
        clippy::cast_precision_loss,
        reason = "an age in seconds, exported as an f64 gauge; exact for any age of interest"
    )]
    let seconds = now.as_second().saturating_sub(stamped.as_second()).max(0) as f64;
    seconds
}

/// Publish the lag of every durable in `durables` and the state of both streams.
///
/// Sampled through the `JetStream` API Edge already holds, so it costs no permission and no port
/// (XR-021 CONTRACTS.md R5-13). Per durable: the messages not yet delivered, the messages delivered
/// and not acknowledged, the redeliveries, and the age of the oldest unacknowledged message, which is
/// the first message after the acknowledgement floor that matches the durable's filter (0 when the
/// durable has caught up). Per stream: messages, bytes, the byte cap and the age of the oldest
/// message.
///
/// A durable that does not exist yet is skipped, and a request that fails is logged and leaves the
/// previous value: a gauge that dropped to zero because the broker blinked would silence the alert
/// that watches it. The `stream` and `durable` labels come from the fixed tables, so the cardinality
/// is bounded by them.
pub async fn sample_bus(
    context: &jetstream::Context,
    durables: &[FixedConsumerSpec],
    now: jiff::Timestamp,
) {
    for name in [COMMAND_STREAM, EVENT_STREAM] {
        if let Err(error) = sample_stream(context, name, now).await {
            tracing::warn!(stream = name, %error, "a stream could not be sampled");
        }
    }
    for spec in durables {
        if let Err(error) = sample_durable(context, spec, now).await {
            tracing::warn!(
                stream = spec.stream,
                durable = spec.durable_name,
                %error,
                "a durable could not be sampled"
            );
        }
    }
}

async fn sample_stream(
    context: &jetstream::Context,
    name: &'static str,
    now: jiff::Timestamp,
) -> Result<(), String> {
    let mut stream = context
        .get_stream(name)
        .await
        .map_err(|error| error.to_string())?;
    let info = stream.info().await.map_err(|error| error.to_string())?;
    let age = if info.state.messages == 0 {
        0.0
    } else {
        age_seconds(now, info.state.first_timestamp)
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "message and byte counts exported as f64 gauges; exact below 2^53"
    )]
    {
        metrics::gauge!(PLATFORM_BUS_STREAM_MESSAGES, "stream" => name)
            .set(info.state.messages as f64);
        metrics::gauge!(PLATFORM_BUS_STREAM_BYTES, "stream" => name).set(info.state.bytes as f64);
        metrics::gauge!(PLATFORM_BUS_STREAM_MAX_BYTES, "stream" => name)
            .set(info.config.max_bytes as f64);
    }
    metrics::gauge!(PLATFORM_BUS_STREAM_OLDEST_MESSAGE_AGE_SECONDS, "stream" => name).set(age);
    Ok(())
}

async fn sample_durable(
    context: &jetstream::Context,
    spec: &FixedConsumerSpec,
    now: jiff::Timestamp,
) -> Result<(), String> {
    let stream = context
        .get_stream(spec.stream)
        .await
        .map_err(|error| error.to_string())?;
    let info = match stream.consumer_info(spec.durable_name).await {
        Ok(info) => info,
        // Not created yet (Edge creates its own consumers when they first run): nothing to say.
        Err(error)
            if matches!(
                error.kind(),
                jetstream::context::ConsumerInfoErrorKind::NotFound
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    };
    let oldest = if info.num_pending == 0 && info.num_ack_pending == 0 {
        0.0
    } else {
        let first_unacked = info.ack_floor.stream_sequence.saturating_add(1);
        match stream
            .raw_message_builder()
            .sequence(first_unacked)
            .next_by_subject(spec.filter_subject)
            .send()
            .await
        {
            Ok(message) => age_seconds(now, message.time),
            Err(error)
                if matches!(
                    error.kind(),
                    jetstream::stream::RawMessageErrorKind::NoMessageFound
                ) =>
            {
                0.0
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    let labels = [("stream", spec.stream), ("durable", spec.durable_name)];
    #[expect(
        clippy::cast_precision_loss,
        reason = "message counts exported as f64 gauges; exact below 2^53"
    )]
    {
        metrics::gauge!(PLATFORM_BUS_CONSUMER_PENDING, &labels).set(info.num_pending as f64);
        metrics::gauge!(PLATFORM_BUS_CONSUMER_ACK_PENDING, &labels)
            .set(info.num_ack_pending as f64);
        metrics::gauge!(PLATFORM_BUS_CONSUMER_REDELIVERED, &labels)
            .set(info.num_redelivered as f64);
    }
    metrics::gauge!(PLATFORM_BUS_CONSUMER_OLDEST_UNACKED_AGE_SECONDS, &labels).set(oldest);
    Ok(())
}
