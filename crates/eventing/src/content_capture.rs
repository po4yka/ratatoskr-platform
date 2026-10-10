//! The `content.capture.requested.v1` command, written once.
//!
//! Three producers ask the extractor to capture a document: `POST /v1/captures`,
//! `POST /v1/captures/blobs` and the webhook adapter. They emit the same typed contract envelope
//! (`ratatoskr_event_envelope::CommandEnvelope` around `ContentCaptureRequested`), built here, so a
//! consumer cannot tell which door a request came through and a field cannot go missing from half
//! the traffic. This replaces the legacy command document for this one command type
//! (XR-021 CONTRACTS.md S11); [`crate::Command`] stays for the commands that still use it.

use ratatoskr_document_contracts::{CaptureUrl, ContentCaptureRequested};
use ratatoskr_event_envelope::{
    CommandEnvelope, CommandPayload as _, EnvelopeSchemaVersion, ProducerName,
};
use ratatoskr_identifiers::{
    BlobRef, CommandId, ContentDigest, DigestAlgorithm, DigestHex, EntityRef, Extensions,
    OperationId, TenantRef, UserId, WireTimestamp,
};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

/// The producer name Platform stamps on the commands it emits.
pub const PLATFORM_PRODUCER: &str = "ratatoskr-platform";

/// What a capture names. Exactly one, as the contract requires.
#[derive(Debug, Clone, Copy)]
pub enum CaptureSource<'a> {
    /// An `http` or `https` address the extractor fetches.
    Url(&'a str),
    /// Bytes another service stored, named by their content address.
    Blob(&'a BlobRef),
}

/// Why a capture command could not be built.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentCaptureError {
    /// A member does not satisfy the contract grammar. The text names the member, never its value.
    #[error("the capture command member {0} does not satisfy the contract")]
    Invalid(&'static str),
}

/// Whether `raw` is an address the capture command can carry: the contract's grammar for a capture
/// URL. A route checks it before it writes anything, so an address the command cannot carry is a
/// client error and not a failure after the operation exists.
#[must_use]
pub fn is_capture_url(raw: &str) -> bool {
    CaptureUrl::parse(raw).is_ok()
}

/// One capture command, ready to be enqueued into the outbox.
#[derive(Debug, Clone, Copy)]
pub struct ContentCaptureCommand<'a> {
    /// The identity of this delivery. The caller uses the same value as the outbox row id, so the
    /// `Nats-Msg-Id` the relay sets is the envelope's own `command_id`.
    pub command_id: Uuid,
    /// The Platform operation the capture belongs to.
    pub operation_id: Uuid,
    /// The user the work is done for.
    pub principal: Uuid,
    /// The request correlation (ADR-0007), carried unchanged, for example `correlation:<uuid>`.
    pub correlation_id: &'a str,
    /// The caller's idempotency string. It is hashed before it is carried, so a key the client
    /// considers private never reaches the bus.
    pub idempotency_key: &'a str,
    /// When the request was accepted.
    pub issued_at: jiff::Timestamp,
    /// What to capture.
    pub source: CaptureSource<'a>,
}

impl ContentCaptureCommand<'_> {
    /// The complete canonical envelope as JSON.
    ///
    /// # Errors
    ///
    /// [`ContentCaptureError::Invalid`] if the address, the correlation or the assembled payload is
    /// outside the contract.
    pub fn envelope(&self) -> Result<serde_json::Value, ContentCaptureError> {
        let hex = DigestHex::parse(&format!(
            "{:x}",
            Sha256::digest(self.idempotency_key.as_bytes())
        ))
        .map_err(|_| ContentCaptureError::Invalid("idempotency_key"))?;
        let (url, blob) = match self.source {
            CaptureSource::Url(url) => (
                Some(CaptureUrl::parse(url).map_err(|_| ContentCaptureError::Invalid("url"))?),
                None,
            ),
            CaptureSource::Blob(blob) => (None, Some(blob.clone())),
        };
        let payload = ContentCaptureRequested {
            operation_id: OperationId(self.operation_id),
            idempotency_key: ContentDigest {
                algorithm: DigestAlgorithm::Sha256,
                hex,
            },
            url,
            blob,
            extensions: Extensions::new(),
        };
        payload
            .validate()
            .map_err(|_| ContentCaptureError::Invalid("source"))?;
        let serde_json::Value::Object(payload) =
            serde_json::to_value(&payload).map_err(|_| ContentCaptureError::Invalid("payload"))?
        else {
            return Err(ContentCaptureError::Invalid("payload"));
        };
        let envelope = CommandEnvelope {
            command_id: CommandId(self.command_id),
            command_type: ContentCaptureRequested::command_type(),
            issued_at: WireTimestamp::from_jiff(self.issued_at),
            producer: ProducerName::parse(PLATFORM_PRODUCER)
                .map_err(|_| ContentCaptureError::Invalid("producer"))?,
            aggregate_id: OperationId(self.operation_id).as_entity_ref(),
            correlation_id: EntityRef::parse(self.correlation_id)
                .map_err(|_| ContentCaptureError::Invalid("correlation_id"))?,
            causation_id: None,
            tenant_id: Some(TenantRef::of_user(UserId(self.principal))),
            schema_version: EnvelopeSchemaVersion::CURRENT,
            payload,
            extensions: Extensions::new(),
        };
        serde_json::to_value(envelope).map_err(|_| ContentCaptureError::Invalid("envelope"))
    }
}
