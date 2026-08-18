//! Moving claimed outbox rows onto the bus.
//!
//! The [`Publisher`] trait exists so the outbox is testable without a broker and so a future
//! transport is a new implementation rather than a rewrite. It is not speculative indirection: the
//! outbox's retry, backoff and dead-letter behaviour must be exercised against a publisher that
//! fails on demand, and no real broker can be made to fail deterministically.

use async_nats::jetstream;

use crate::Subject;

/// Why a publication did not happen.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// The broker refused, was unreachable, or did not acknowledge.
    #[error("the message was not acknowledged by the bus")]
    NotAcknowledged(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// Something that can put a message on the bus.
pub trait Publisher: Send + Sync {
    /// Publish and wait for the broker to take responsibility for the message.
    ///
    /// Waiting for the acknowledgement is the whole contract: a fire-and-forget publish would let
    /// the outbox mark a row published that the broker never stored, which converts at-least-once
    /// into at-most-once silently.
    fn publish(
        &self,
        subject: &Subject,
        payload: &[u8],
        message_id: &str,
    ) -> impl Future<Output = Result<(), PublishError>> + Send;
}

/// A `JetStream` publisher.
#[derive(Debug, Clone)]
pub struct NatsPublisher {
    context: jetstream::Context,
}

impl NatsPublisher {
    /// Wrap a `JetStream` context.
    #[must_use]
    pub const fn new(context: jetstream::Context) -> Self {
        Self { context }
    }

    /// Connect to a NATS server and take its `JetStream` context.
    ///
    /// # Errors
    ///
    /// [`PublishError::NotAcknowledged`] if the server is unreachable.
    pub async fn connect(url: &str) -> Result<Self, PublishError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|error| PublishError::NotAcknowledged(Box::new(error)))?;
        Ok(Self::new(jetstream::new(client)))
    }

    /// The `JetStream` context, for stream and consumer management.
    #[must_use]
    pub const fn context(&self) -> &jetstream::Context {
        &self.context
    }
}

impl Publisher for NatsPublisher {
    async fn publish(
        &self,
        subject: &Subject,
        payload: &[u8],
        message_id: &str,
    ) -> Result<(), PublishError> {
        // `Nats-Msg-Id` is what makes JetStream's own duplicate window work: a redelivery of the
        // same outbox row inside the window is collapsed by the server, so the consumer's inbox is
        // the second line of defence rather than the only one.
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Msg-Id", message_id);

        let ack = self
            .context
            .publish_with_headers(
                subject.as_str().to_owned(),
                headers,
                payload.to_vec().into(),
            )
            .await
            .map_err(|error| PublishError::NotAcknowledged(Box::new(error)))?;

        ack.await
            .map_err(|error| PublishError::NotAcknowledged(Box::new(error)))?;
        Ok(())
    }
}
