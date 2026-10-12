//! Erasing single stream messages at a producer's request (XR-021 CONTRACTS.md R5-12).
//!
//! `ratatoskr_events` keeps a conversation, project or artifact fact, with the full message text on
//! the wire, for up to seven days after its owner deleted it, and no producer may remove it: Edge is
//! the only identity with `$JS.API.>`, and no other holds any stream-management subject. A producer
//! therefore sends `platform.stream_erasure.requested.v1` naming the stream sequences the broker
//! acknowledged when it published them, and this handler, running inside Edge, decides for each one.
//!
//! A sequence is erased only when all three of these hold, so a producer reaches nothing but its own
//! facts for the owner it names:
//!
//! 1. the stored subject is one of the seven content-bearing `evt.ai_archive.*` subjects, never the
//!    tombstone, which carries no content and is the fact Knowledge acts on;
//! 2. the stored envelope's `producer` is the command's producer;
//! 3. the stored envelope's `tenant_id` is the command's `owner`.
//!
//! Anything else that exists is counted `refused` and left alone, and a sequence that no longer
//! exists is counted `missing`. The handler runs inside the inbox transaction, so a replay of the
//! same command id is absorbed before any broker request is made.

use async_nats::jetstream::{self, stream::RawMessageErrorKind};
use jiff::Timestamp;
use platform_identity::audit::{self, AuditEvent, AuditOutcome};
use platform_persistence::PersistenceError;
use platform_telemetry::metrics::PLATFORM_STREAM_ERASURE_TOTAL;
use ratatoskr_event_envelope::CommandEnvelope;
use ratatoskr_operation_contracts::PlatformStreamErasureRequested;
use uuid::Uuid;

use crate::consumer::Handled;
use crate::inbox::Outcome;
use crate::{EVENT_STREAM, EventingError, Handler, Incoming, to_offset};

/// The command subject Edge consumes.
pub const SUBJECT: &str = "cmd.platform.stream_erasure.requested.v1";

/// The durable Edge's own consumer uses. Created by the consumer, not by the fixed table.
pub const DURABLE: &str = "platform_stream_erasure";

/// The producers whose commands are honoured: the two AI archive importers.
const PRODUCERS: [&str; 2] = ["ratatoskr-chatgpt", "ratatoskr-claude"];

/// The seven subjects whose facts carry content and may be erased. The tombstone is not among them.
const ERASABLE_SUBJECTS: [&str; 7] = [
    "evt.ai_archive.archive.imported.v1",
    "evt.ai_archive.conversation.added.v1",
    "evt.ai_archive.conversation.updated.v1",
    "evt.ai_archive.project.added.v1",
    "evt.ai_archive.project.updated.v1",
    "evt.ai_archive.artifact.added.v1",
    "evt.ai_archive.artifact.updated.v1",
];

/// Erases the stream messages a producer names, when they are its own.
#[derive(Debug, Clone)]
pub struct StreamErasureHandler {
    context: jetstream::Context,
}

impl StreamErasureHandler {
    /// A handler that talks to the broker through `context`, Edge's own connection.
    #[must_use]
    pub const fn new(context: jetstream::Context) -> Self {
        Self { context }
    }

    /// Decide and, if allowed, erase one sequence.
    async fn dispose(
        &self,
        stream: &jetstream::stream::Stream,
        request: &Request,
        sequence: u64,
    ) -> Result<Disposition, EventingError> {
        let stored = match stream.get_raw_message(sequence).await {
            Ok(stored) => stored,
            Err(error) if matches!(error.kind(), RawMessageErrorKind::NoMessageFound) => {
                return Ok(Disposition::Missing);
            }
            Err(error) => return Err(EventingError::Bus(error.to_string())),
        };
        if !request.owns(stored.subject.as_str(), &stored.payload) {
            return Ok(Disposition::Refused);
        }
        match stream.delete_message(sequence).await {
            Ok(true) => Ok(Disposition::Erased),
            // The server answered that nothing was deleted: it went between the read and the delete.
            Ok(false) => Ok(Disposition::Missing),
            Err(error) => match error.kind() {
                jetstream::stream::DeleteMessageErrorKind::JetStream(detail)
                    if detail.error_code() == jetstream::ErrorCode::NO_MESSAGE_FOUND =>
                {
                    Ok(Disposition::Missing)
                }
                _ => Err(EventingError::Bus(error.to_string())),
            },
        }
    }
}

/// What happened to one named sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Erased,
    Missing,
    Refused,
}

/// How many sequences of a command went each way.
#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    erased: i32,
    missing: i32,
    refused: i32,
}

impl Counts {
    fn add(&mut self, disposition: Disposition) {
        match disposition {
            Disposition::Erased => self.erased += 1,
            Disposition::Missing => self.missing += 1,
            Disposition::Refused => self.refused += 1,
        }
    }
}

/// A command that passed every check of the envelope.
#[derive(Debug)]
struct Request {
    producer: String,
    owner: String,
    reference: String,
    correlation: String,
    sequences: Vec<u64>,
}

impl Request {
    /// The request a delivered message carries, or `None` for anything that is not exactly the
    /// contract command: the subject, the whole `CommandEnvelope`, the producer, the tenant, the
    /// aggregate and the typed payload are all checked. A `None` is permanent, so it is Termed.
    fn read(message: &Incoming) -> Option<Self> {
        if message.subject.as_str() != SUBJECT {
            return None;
        }
        let bytes = serde_json::to_vec(&message.payload).ok()?;
        let envelope = CommandEnvelope::from_json(&bytes).ok()?;
        let producer = envelope.producer.as_str();
        if !PRODUCERS.contains(&producer) || producer != message.producer {
            return None;
        }
        let body: PlatformStreamErasureRequested = envelope.payload_as().ok()?;
        if envelope.tenant_id != Some(body.owner) || envelope.aggregate_id != body.request_ref {
            return None;
        }
        Some(Self {
            producer: producer.to_owned(),
            owner: body.owner.to_string(),
            reference: body.request_ref.to_string(),
            correlation: envelope.correlation_id.to_string(),
            sequences: body.stream_sequences,
        })
    }

    /// Whether a stored message is one this request may erase.
    fn owns(&self, subject: &str, payload: &[u8]) -> bool {
        if !ERASABLE_SUBJECTS.contains(&subject) {
            return false;
        }
        let Ok(stored) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return false;
        };
        let text = |member: &str| stored.get(member).and_then(serde_json::Value::as_str);
        text("producer") == Some(self.producer.as_str())
            && text("tenant_id") == Some(self.owner.as_str())
    }
}

impl Handler for StreamErasureHandler {
    async fn handle(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
        now: Timestamp,
    ) -> Result<Handled, EventingError> {
        let Some(request) = Request::read(message) else {
            // Log the class only, never the content of the message.
            tracing::warn!(
                class = "stream_erasure_rejected",
                "a stream erasure command did not pass validation and was Termed",
            );
            return Ok(Outcome::Rejected);
        };
        let stream = self
            .context
            .get_stream(EVENT_STREAM)
            .await
            .map_err(|error| EventingError::Bus(error.to_string()))?;
        let mut counts = Counts::default();
        for &sequence in &request.sequences {
            counts.add(self.dispose(&stream, &request, sequence).await?);
        }
        record(transaction, message.message_id, &request, counts, now).await?;
        for (outcome, count) in [
            ("erased", counts.erased),
            ("missing", counts.missing),
            ("refused", counts.refused),
        ] {
            metrics::counter!(PLATFORM_STREAM_ERASURE_TOTAL, "outcome" => outcome)
                .increment(u64::try_from(count).unwrap_or(0));
        }
        Ok(Outcome::Applied)
    }
}

/// Write the content-free audit row of one command, and the shared audit-trail event it joins to.
async fn record(
    transaction: &mut sqlx::PgTransaction<'_>,
    command_id: Uuid,
    request: &Request,
    counts: Counts,
    now: Timestamp,
) -> Result<(), EventingError> {
    let audit_event_id = Uuid::now_v7();
    audit::record(
        &mut **transaction,
        &AuditEvent {
            audit_event_id,
            actor_user_id: None,
            actor_session_id: None,
            action: "stream.erasure",
            target_kind: "privacy_request",
            target_id: None,
            outcome: if counts.refused == 0 {
                AuditOutcome::Allowed
            } else {
                AuditOutcome::Denied
            },
            correlation_id: request.correlation.clone(),
        },
        now,
    )
    .await?;
    sqlx::query(
        "insert into operations.stream_erasure_audit
             (command_id, audit_event_id, producer, owner, request_ref, erased, missing, refused,
              recorded_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(command_id)
    .bind(audit_event_id)
    .bind(&request.producer)
    .bind(&request.owner)
    .bind(&request.reference)
    .bind(counts.erased)
    .bind(counts.missing)
    .bind(counts.refused)
    .bind(to_offset(now))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(())
}
