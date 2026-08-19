//! Moving claimed outbox rows onto the bus.
//!
//! The [`Publisher`] trait exists so the outbox is testable without a broker and so a future
//! transport is a new implementation rather than a rewrite. It is not speculative indirection: the
//! outbox's retry, backoff and dead-letter behaviour must be exercised against a publisher that
//! fails on demand, and no real broker can be made to fail deterministically.

use async_nats::jetstream;

use crate::stream::StreamState;
use crate::{StreamSpec, Subject};

/// Why a publication did not happen.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// The broker refused, was unreachable, or did not acknowledge.
    #[error("the message was not acknowledged by the bus")]
    NotAcknowledged(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// The nkey seed file could not be read. Its path is deliberately absent from the message: the
    /// `source` carries what the operating system said, and the configured path is already in the
    /// effective-configuration log line.
    #[error("the bus credential could not be read")]
    Credential(#[source] std::io::Error),
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
    client: async_nats::Client,
    context: jetstream::Context,
}

impl NatsPublisher {
    /// Wrap a connected client.
    ///
    /// The client is kept beside the context it produced, because `jetstream::Context` does not
    /// expose the connection it uses and [`Self::is_connected`] is the readiness input for the bus.
    #[must_use]
    pub fn new(client: async_nats::Client) -> Self {
        Self {
            context: jetstream::new(client.clone()),
            client,
        }
    }

    /// Whether the client currently holds a connection to a server.
    ///
    /// Reads the client's own state and performs no I/O: `async-nats` reconnects on its own and
    /// tracks where it is, so asking is free and asking often is free. A probe that published a
    /// message to find out would put load on the broker in exactly the situation where the broker
    /// is already the problem.
    ///
    /// `Pending` — reconnecting — reports `false`. The question readiness asks is whether this
    /// process can reach the bus now, and "it is trying" is not a yes.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(
            self.client.connection_state(),
            async_nats::connection::State::Connected
        )
    }

    /// Connect to a NATS server anonymously and take its `JetStream` context.
    ///
    /// Anonymous is what `compose.yaml` serves and what no deployment should: see
    /// [`Self::connect_with_nkey`].
    ///
    /// # Errors
    ///
    /// [`PublishError::NotAcknowledged`] if the server is unreachable.
    pub async fn connect(url: &str) -> Result<Self, PublishError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|error| PublishError::NotAcknowledged(Box::new(error)))?;
        Ok(Self::new(client))
    }

    /// Connect as the identity whose nkey seed is in `seed_path`.
    ///
    /// The seed is read here and handed straight to the client rather than being held anywhere: it
    /// lives for the life of the connection inside `async-nats`, which needs it to sign every
    /// server nonce, and a second copy in a configuration struct would be a second thing that can
    /// be logged.
    ///
    /// Trailing whitespace is stripped because the file is written by a human or by `nk -gen user`,
    /// and a trailing newline would otherwise become an authentication failure whose message names
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`PublishError::Credential`] if the seed file cannot be read, and
    /// [`PublishError::NotAcknowledged`] if the server is unreachable or rejects the identity.
    pub async fn connect_with_nkey(
        url: &str,
        seed_path: &std::path::Path,
    ) -> Result<Self, PublishError> {
        let seed = std::fs::read_to_string(seed_path).map_err(PublishError::Credential)?;
        let client = async_nats::ConnectOptions::with_nkey(seed.trim().to_owned())
            .connect(url)
            .await
            .map_err(|error| PublishError::NotAcknowledged(Box::new(error)))?;
        Ok(Self::new(client))
    }

    /// The `JetStream` context, for stream and consumer management.
    #[must_use]
    pub const fn context(&self) -> &jetstream::Context {
        &self.context
    }

    /// Make sure the stream `spec` describes exists.
    ///
    /// `JetStream` does not acknowledge a publish to a subject no stream is bound to, and the
    /// publisher treats an unacknowledged publish as a failure — correctly, since the message was
    /// not stored. Without a stream the outbox therefore retries, backs off and eventually
    /// dead-letters perfectly good commands, with a diagnosis ("not acknowledged") that does not
    /// name the cause.
    ///
    /// Idempotent. There is one publisher on one host, so this process is the right owner of the
    /// topology it publishes to (ADR-0010). The deployment profile does NOT take the limits over:
    /// ADR-0013 puts the names and the limits in [`crate::stream`] and has `deploy/` transcribe
    /// them, so there is one source for a value a permission file and a runbook both repeat.
    ///
    /// A stream that already exists is NOT reconciled — that is the `JetStream` client's behaviour,
    /// not a choice here — so the returned [`StreamState`] names every limit that differs. Changing
    /// one is an operator action against the broker, not a redeploy.
    ///
    /// # Errors
    ///
    /// [`PublishError::NotAcknowledged`] if the stream cannot be created.
    pub async fn ensure_stream(&self, spec: &StreamSpec) -> Result<StreamState, PublishError> {
        crate::stream::ensure(&self.context, spec)
            .await
            .map_err(|error| PublishError::NotAcknowledged(Box::new(error)))
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
