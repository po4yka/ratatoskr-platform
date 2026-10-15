//! Receiving events from the bus and handing them to a handler exactly once.
//!
//! The deduplication is the inbox, not the broker. `JetStream`'s duplicate window collapses a
//! redelivery inside it, but the window is finite and a consumer that restarts after it has passed
//! will see the message again. `ARCHITECTURE.md` S19 invariant 7 assumes at-least-once, so the
//! durable answer is a row in `operations.inbox` written in the SAME transaction as the state change
//! the message causes.

use async_nats::jetstream;
use futures_util::StreamExt as _;
use sqlx::PgPool;
use uuid::Uuid;

use crate::inbox::Outcome;
use crate::{EventingError, Inbox, Reception, StreamSpec, Subject};

/// How long the broker waits before redelivering a message whose handler failed transiently.
const TRANSIENT_FAILURE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// What a handler decided about one message.
///
/// Returned rather than logged so the inbox records it: `operations.inbox.outcome` is how an
/// operator sees that redeliveries are being absorbed rather than dropped.
pub type Handled = Outcome;

/// The envelope members this crate needs to route a message. Everything else is the handler's.
#[derive(Debug, Clone)]
pub struct Incoming {
    /// The producer's message identity, and the deduplication key.
    pub message_id: Uuid,
    /// Where it arrived.
    pub subject: Subject,
    /// Who sent it.
    pub producer: String,
    /// The whole envelope, for the handler.
    pub payload: serde_json::Value,
}

/// What a consumer does with a message.
pub trait Handler: Send + Sync {
    /// Apply the message inside `transaction`.
    ///
    /// The transaction already holds the inbox row, so the handler's writes and the record that the
    /// message was seen commit together. A handler that fails leaves neither. `now` is the instant
    /// [`deliver`] was given, so a handler never reads the wall clock itself.
    fn handle(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
        now: jiff::Timestamp,
    ) -> impl Future<Output = Result<Handled, EventingError>> + Send;

    /// Called by [`deliver`] once the transaction of a message that reached
    /// [`Handler::handle`] is settled: `committed` when it committed, else it rolled back. A
    /// handler with an effect outside the database (a wake-up of open streams) applies it here,
    /// so the effect never comes before the commit or after a rollback.
    fn settled(&self, _message: &Incoming, _committed: bool) {}
}

/// Deduplicate a message, hand it to `handler`, and record what happened — all in one transaction.
///
/// Returns `None` when the message was already processed, so a caller can count duplicates without
/// having to ask the inbox a second question.
///
/// # Errors
///
/// [`EventingError::Persistence`] if a statement fails, or whatever the handler returns.
pub async fn deliver<H: Handler>(
    pool: &PgPool,
    handler: &H,
    message: &Incoming,
    now: jiff::Timestamp,
) -> Result<Option<Handled>, EventingError> {
    let mut transaction = pool.begin().await.map_err(|error| {
        EventingError::Persistence(platform_persistence::PersistenceError::Query(error))
    })?;

    let reception = Inbox::begin(
        &mut *transaction,
        message.message_id,
        &message.subject,
        &message.producer,
        now,
    )
    .await?;

    if reception == Reception::Duplicate {
        // Nothing to undo, but the transaction is still rolled back rather than committed: it did
        // not write anything, and committing an empty transaction would only cost a round trip.
        drop(transaction);
        return Ok(None);
    }

    let applied = async move {
        let outcome = handler.handle(&mut transaction, message, now).await?;
        Inbox::finish(&mut *transaction, message.message_id, outcome, now).await?;
        transaction.commit().await.map_err(|error| {
            EventingError::Persistence(platform_persistence::PersistenceError::Query(error))
        })?;
        Ok(outcome)
    }
    .await;
    handler.settled(message, applied.is_ok());

    applied.map(Some)
}

/// Subscribe to `subjects` on a durable `JetStream` consumer and deliver every message to `handler`
/// until `stop` resolves.
///
/// Durable, not ephemeral: a restart must resume where the process left off. An ephemeral consumer
/// would silently skip everything published while it was down, which looks like working software
/// until an operation never completes.
///
/// # Errors
///
/// [`EventingError::Persistence`] if the stream or consumer cannot be created.
pub async fn run<H: Handler>(
    context: &jetstream::Context,
    spec: &StreamSpec,
    durable_name: &str,
    filter_subject: &str,
    pool: &PgPool,
    handler: &H,
    stop: impl Future<Output = ()> + Send,
) -> Result<ConsumerReport, EventingError> {
    run_filters(
        context,
        spec,
        durable_name,
        &[filter_subject],
        pool,
        handler,
        stop,
    )
    .await
}

/// [`run`] for a durable with several filter subjects, such as `platform_site_events`
/// (workspace change `add-site-owner-editing-and-live`). One filter is set as the filter subject;
/// more are set as the filter subjects of the consumer, which NATS 2.10 introduced.
///
/// # Errors
///
/// [`EventingError::Persistence`] if the stream or consumer cannot be created.
pub async fn run_filters<H: Handler>(
    context: &jetstream::Context,
    spec: &StreamSpec,
    durable_name: &str,
    filters: &[&str],
    pool: &PgPool,
    handler: &H,
    stop: impl Future<Output = ()> + Send,
) -> Result<ConsumerReport, EventingError> {
    // The same spec the publisher would declare, for the same reason: whichever process reaches the
    // broker first creates the stream, and a stream created from `Config::default()` retains
    // everything and then silently drops the oldest. Two declaration sites, one policy.
    if let crate::stream::StreamState::Existing { mismatches } =
        crate::stream::ensure(context, spec).await?
        && !mismatches.is_empty()
    {
        tracing::warn!(
            stream = %spec.name,
            mismatches = ?mismatches,
            "the stream on the broker was created with different limits and was NOT reconciled; \
             update or delete it"
        );
    }
    let stream = context
        .get_stream(&spec.name)
        .await
        .map_err(|error| EventingError::Bus(error.to_string()))?;

    let consumer = stream
        .get_or_create_consumer(
            durable_name,
            jetstream::consumer::pull::Config {
                durable_name: Some(durable_name.to_owned()),
                filter_subject: match filters {
                    [one] => (*one).to_owned(),
                    _ => String::new(),
                },
                filter_subjects: match filters {
                    [_] => Vec::new(),
                    many => many.iter().map(|filter| (*filter).to_owned()).collect(),
                },
                ..jetstream::consumer::pull::Config::default()
            },
        )
        .await
        .map_err(|error| EventingError::Bus(error.to_string()))?;

    let mut messages = consumer
        .messages()
        .await
        .map_err(|error| EventingError::Bus(error.to_string()))?;

    let mut report = ConsumerReport::default();
    tokio::pin!(stop);

    loop {
        let message = tokio::select! {
            biased;
            () = &mut stop => break,
            next = messages.next() => next,
        };
        let Some(message) = message else { break };
        let Ok(message) = message else {
            report.malformed += 1;
            continue;
        };

        match parse(&message) {
            Some(incoming) => {
                match deliver(pool, handler, &incoming, jiff::Timestamp::now()).await {
                    Ok(Some(_)) => report.applied += 1,
                    Ok(None) => report.duplicates += 1,
                    Err(error) => {
                        // Not acknowledged: `JetStream` redelivers, and the inbox makes that safe.
                        // Acknowledging a message the handler could not apply would lose the work.
                        // A handler returns an error only for a transient failure (a rejection is an
                        // outcome, not an error), so the message is Nak'd with a short delay instead
                        // of waiting out the whole ack wait (XR-021 CONTRACTS.md S02 rule 7).
                        report.failed += 1;
                        tracing::error!(%error, "an event could not be applied");
                        if let Err(error) = message
                            .ack_with(jetstream::AckKind::Nak(Some(TRANSIENT_FAILURE_DELAY)))
                            .await
                        {
                            tracing::warn!(%error, "an event could not be negatively acknowledged");
                        }
                        continue;
                    }
                }
            }
            None => {
                // A message this build cannot read is acknowledged, not redelivered forever: the
                // shape will not improve on the next attempt, and a poison message must not stall
                // the consumer. It is counted so it is visible.
                report.malformed += 1;
            }
        }

        if let Err(error) = message.ack().await {
            tracing::warn!(%error, "an event could not be acknowledged");
        }
    }

    Ok(report)
}

/// What one consumer run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConsumerReport {
    /// Messages applied for the first time.
    pub applied: usize,
    /// Messages already recorded in the inbox.
    pub duplicates: usize,
    /// Messages the handler refused.
    pub failed: usize,
    /// Messages this build could not read.
    pub malformed: usize,
}

/// Lift the members needed for routing out of a raw message.
fn parse(message: &jetstream::Message) -> Option<Incoming> {
    let payload: serde_json::Value = serde_json::from_slice(&message.payload).ok()?;
    let message_id = payload
        .get("event_id")
        .or_else(|| payload.get("command_id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| raw.parse::<Uuid>().ok())?;
    let producer = payload
        .get("producer")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    Some(Incoming {
        message_id,
        subject: Subject::parse(&message.subject).ok()?,
        producer,
        payload,
    })
}
