//! Applying an inbound progress event to the operation projection.
//!
//! This is the consuming half of `ARCHITECTURE.md` S5.1: a domain service reports progress, and
//! Platform maintains the public projection a client polls or streams. Platform consumes only the
//! members it needs — S11.2: "Platform consumes only the fields required to maintain public
//! projections. It does not duplicate complete domain records."

use platform_eventing::inbox::Outcome;
use platform_eventing::{EventingError, Handler, Incoming};
use ratatoskr_operation_contracts::OperationStatus;
use uuid::Uuid;

use crate::transition::Transition;

/// Applies `platform.operation.progressed.v1`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProgressProjection;

impl Handler for ProgressProjection {
    async fn handle(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
    ) -> Result<Outcome, EventingError> {
        let Some(report) = ProgressReport::read(&message.payload) else {
            // A message this build cannot read is recorded as rejected rather than retried: its
            // shape will not improve on redelivery, and the inbox row is what makes it visible.
            return Ok(Outcome::Rejected);
        };

        let now = jiff::Timestamp::now();
        let outcome = crate::record_status(
            transaction,
            report.operation_id,
            report.status,
            report.stage.as_deref(),
            report.progress_percent,
            report.message.as_deref(),
            now,
        )
        .await;

        let (transition, _) = match outcome {
            Ok(applied) => applied,
            // Two producers claiming different terminal outcomes is the one case that is an alarm,
            // and it is recorded rather than retried: redelivering will produce the same conflict.
            Err(crate::OperationError::ConflictingOutcome { current, incoming }) => {
                tracing::error!(
                    operation_id = %report.operation_id,
                    current,
                    incoming,
                    "two producers reported different terminal outcomes",
                );
                return Ok(Outcome::Rejected);
            }
            // An event for an operation this Platform does not have is not an error either: it is
            // an event for somebody else's operation, or one already collected.
            Err(crate::OperationError::NotFound) => return Ok(Outcome::Rejected),
            Err(crate::OperationError::Persistence(error)) => {
                return Err(EventingError::Persistence(error));
            }
            Err(error) => {
                tracing::error!(%error, "a progress event could not be applied");
                return Ok(Outcome::Rejected);
            }
        };

        for result in report.results {
            crate::record_result(
                &mut **transaction,
                report.operation_id,
                &result.result_kind,
                &result.target,
                None,
                now,
            )
            .await
            .map_err(|error| match error {
                crate::OperationError::Persistence(error) => EventingError::Persistence(error),
                other => EventingError::Bus(other.to_string()),
            })?;
        }

        Ok(match transition {
            Transition::Advance(_) => Outcome::Applied,
            Transition::Duplicate => Outcome::Duplicate,
            Transition::Stale => Outcome::Stale,
            // `record_status` cannot return `Conflict` here; it is handled above.
            Transition::Conflict => Outcome::Rejected,
        })
    }
}

/// A typed result reference carried by a progress event.
struct ReportedResult {
    result_kind: String,
    target: String,
}

/// The members Platform reads from a progress event, and no others.
struct ProgressReport {
    operation_id: Uuid,
    status: OperationStatus,
    stage: Option<String>,
    progress_percent: Option<u8>,
    message: Option<String>,
    results: Vec<ReportedResult>,
}

impl ProgressReport {
    /// Read the envelope, returning `None` for anything this build cannot act on.
    ///
    /// Permissive about members it does not use and strict about the two it does: an event with no
    /// operation or an unrecognised status cannot be applied, and guessing would corrupt the
    /// projection.
    fn read(payload: &serde_json::Value) -> Option<Self> {
        let body = payload.get("payload").unwrap_or(payload);
        let operation_id = body
            .get("operation_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| raw.parse().ok())?;
        let status = body
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(status_from_token)?;

        let results = body
            .get("results")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(ReportedResult {
                            result_kind: item
                                .get("result_kind")
                                .and_then(serde_json::Value::as_str)?
                                .to_owned(),
                            target: item
                                .get("target")
                                .and_then(serde_json::Value::as_str)?
                                .to_owned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Some(Self {
            operation_id,
            status,
            stage: body
                .get("stage")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            progress_percent: body
                .get("progress_percent")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .filter(|value| *value <= 100),
            message: body
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            results,
        })
    }
}

fn status_from_token(token: &str) -> Option<OperationStatus> {
    match token {
        "accepted" => Some(OperationStatus::Accepted),
        "queued" => Some(OperationStatus::Queued),
        "running" => Some(OperationStatus::Running),
        "succeeded" => Some(OperationStatus::Succeeded),
        "partially_succeeded" => Some(OperationStatus::PartiallySucceeded),
        "failed" => Some(OperationStatus::Failed),
        "cancelled" => Some(OperationStatus::Cancelled),
        _ => None,
    }
}
