//! Projecting the conflict events of `ratatoskr-notes` onto the write operations (workspace change
//! `add-notes-write-back`).
//!
//! `evt.notes.conflict.raised.v1` names the `notes.file.write` operation that ended in a conflict,
//! and `evt.notes.conflict.resolved.v1` names the operation of the `resolve_conflict` write that
//! closed it. Edge records each as a result reference of that operation, `notes.conflict.raised` or
//! `notes.conflict.resolved` with the target `notes_conflict:<id>`, so that a client that follows
//! the operation finds the conflict to read at `GET /v1/notes/conflicts/{conflict_id}`. The events
//! carry hashes only, and the projection keeps none of them: only the conflict reference.
//!
//! An event is applied only when the operation exists, is a `notes.file.write`, and the report
//! authority would accept a report from the event's producer and tenant for it: the producer is
//! `ratatoskr-notes`, the lane recorded on the operation, and the tenant is its owner.

use platform_eventing::inbox::Outcome;
use platform_eventing::{EventingError, Handler, Incoming};
use ratatoskr_identifiers::{Extensions, NotesConflictId, OperationId};
use ratatoskr_notes_contracts::{ConflictRaised, ConflictResolved};
use ratatoskr_operation_contracts::{OperationResultKind, OperationResultRef};

/// The only kind of operation that a conflict event names.
const WRITE_KIND: &str = "notes.file.write";

/// Applies `notes.conflict.raised.v1` and `notes.conflict.resolved.v1`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NotesConflictProjection;

impl Handler for NotesConflictProjection {
    async fn handle(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
        now: jiff::Timestamp,
    ) -> Result<Outcome, EventingError> {
        // A message this build cannot read is recorded as rejected rather than retried: its shape
        // will not improve on redelivery, and the inbox row is what makes it visible.
        let Some((operation_id, conflict_id, result_kind)) = read(message) else {
            return Ok(Outcome::Rejected);
        };
        let Some(operation) = crate::find(&mut **transaction, operation_id.0)
            .await
            .map_err(map_error)?
        else {
            return Ok(Outcome::Rejected);
        };
        let tenant = message
            .payload
            .get("tenant_id")
            .and_then(serde_json::Value::as_str);
        if operation.kind != WRITE_KIND
            || !crate::authority::authorize(
                &operation.kind,
                &message.producer,
                tenant,
                operation.owner_user_id,
                operation.reporter.as_deref(),
                false,
            )
        {
            tracing::warn!(
                operation_id = %operation_id.0,
                class = "conflict_not_authorized",
                "a notes conflict event was rejected",
            );
            return Ok(Outcome::Rejected);
        }

        let target = conflict_id.as_entity_ref();
        let recorded: bool = sqlx::query_scalar(
            "select exists (select 1 from operations.operation_results
                             where operation_id = $1 and result_kind = $2 and target = $3)",
        )
        .bind(operation_id.0)
        .bind(result_kind)
        .bind(String::from(target.clone()))
        .fetch_one(&mut **transaction)
        .await
        .map_err(|error| {
            EventingError::Persistence(platform_persistence::PersistenceError::Query(error))
        })?;
        if recorded {
            return Ok(Outcome::Duplicate);
        }
        let result = OperationResultRef {
            result_kind: OperationResultKind::parse(result_kind)
                .map_err(|error| EventingError::Bus(error.to_string()))?,
            target,
            blob: None,
            ai_archive_import_summary: None,
            extensions: Extensions::new(),
        };
        crate::record_result(&mut **transaction, operation_id.0, &result, now)
            .await
            .map_err(map_error)?;
        Ok(Outcome::Applied)
    }
}

/// The operation, the conflict and the result kind of a conflict event, or `None` for a message
/// that is not one.
fn read(message: &Incoming) -> Option<(OperationId, NotesConflictId, &'static str)> {
    let payload = message.payload.get("payload")?.clone();
    match message.subject.as_str() {
        "evt.notes.conflict.raised.v1" => {
            let event: ConflictRaised = serde_json::from_value(payload).ok()?;
            Some((
                event.operation_id,
                event.conflict_id,
                "notes.conflict.raised",
            ))
        }
        "evt.notes.conflict.resolved.v1" => {
            let event: ConflictResolved = serde_json::from_value(payload).ok()?;
            Some((
                event.operation_id,
                event.conflict_id,
                "notes.conflict.resolved",
            ))
        }
        _ => None,
    }
}

fn map_error(error: crate::OperationError) -> EventingError {
    match error {
        crate::OperationError::Persistence(error) => EventingError::Persistence(error),
        other => EventingError::Bus(other.to_string()),
    }
}
