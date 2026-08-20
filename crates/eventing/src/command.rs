//! The command envelope, written once.
//!
//! `ARCHITECTURE.md` S5.3 fixes what a command carries: "Commands include principal, operation,
//! correlation, causation, idempotency, and schema version" (`INTERFACES.md` repeats it). Two route
//! families now emit commands — `POST /v1/captures` and the webhook adapter of milestone 7 — and a
//! consumer must not be able to tell which one produced the message it is reading.
//!
//! That is why this lives in `ratatoskr-eventing` rather than beside either route: the envelope is
//! the wire shape of a command, the same way [`crate::Subject`] is the wire shape of its address,
//! and a second hand-written copy of it beside a second route is the drift that would first show up
//! as a field silently missing from half the traffic.
//!
//! A typed envelope from `ratatoskr-contracts` replaces this the day contracts ships one. Until
//! then the members below are exactly S5.3's list and nothing else.

use serde_json::json;
use uuid::Uuid;

/// One command, ready to be enqueued into the outbox.
///
/// Borrowed rather than owned: every member comes from something the handler already holds, and
/// copying a correlation identifier to build a JSON document it is immediately serialized into
/// would be two allocations for no reader.
#[derive(Debug, Clone, Copy)]
pub struct Command<'a> {
    /// The contract type name, e.g. `content.capture.requested.v1`. Its version is the schema
    /// version S5.3 requires, and [`crate::Subject`] validates the same string.
    pub command_type: &'a str,
    /// The operation this command belongs to. The causation link: an event referring to this
    /// operation is the answer to this command.
    pub operation_id: Uuid,
    /// The principal on whose behalf the work is requested.
    pub principal: Uuid,
    /// The correlation identifier the request was minted with (ADR-0007).
    pub correlation_id: &'a str,
    /// The idempotency key the caller supplied, so a consumer that retries can be recognised.
    pub idempotency_key: &'a str,
    /// When the request was accepted.
    pub requested_at: jiff::Timestamp,
}

impl Command<'_> {
    /// The document, with `payload` as its domain half.
    ///
    /// `payload` is the only part that differs between command families; everything above it is
    /// identical for all of them, which is the entire reason this function exists.
    #[must_use]
    pub fn envelope(&self, payload: serde_json::Value) -> serde_json::Value {
        // Built member by member rather than through `json!`, so `payload` is MOVED in rather than
        // re-serialized from a borrow. A domain payload can be large, and copying one to place it
        // inside the document it is about would be a copy no reader benefits from.
        let mut envelope = serde_json::Map::new();
        envelope.insert("command_id".to_owned(), json!(Uuid::now_v7()));
        envelope.insert("command_type".to_owned(), json!(self.command_type));
        envelope.insert(
            "requested_at".to_owned(),
            json!(self.requested_at.to_string()),
        );
        envelope.insert("operation_id".to_owned(), json!(self.operation_id));
        envelope.insert(
            "tenant_id".to_owned(),
            json!(format!("user:{}", self.principal)),
        );
        envelope.insert("correlation_id".to_owned(), json!(self.correlation_id));
        envelope.insert("idempotency_key".to_owned(), json!(self.idempotency_key));
        envelope.insert("payload".to_owned(), payload);
        serde_json::Value::Object(envelope)
    }
}
