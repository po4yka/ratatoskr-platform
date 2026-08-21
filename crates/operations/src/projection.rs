//! Applying an inbound progress report to the operation projection.
//!
//! This is the consuming half of `ARCHITECTURE.md` S5.1: a domain service reports progress, and
//! Platform maintains the public projection a client polls or streams. Platform consumes only the
//! members it needs — S11.2: "Platform consumes only the fields required to maintain public
//! projections. It does not duplicate complete domain records."

use platform_eventing::inbox::Outcome;
use platform_eventing::{EventingError, Handler, Incoming};
use ratatoskr_operation_contracts::{OperationReported, OperationStatus};
use uuid::Uuid;

use crate::transition::Transition;

/// Applies `platform.operation.reported.v1`.
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
            None,
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

/// A typed result reference carried by a progress report.
struct ReportedResult {
    result_kind: String,
    target: String,
}

/// The members Platform reads from a progress report, and no others.
struct ProgressReport {
    operation_id: Uuid,
    status: OperationStatus,
    stage: Option<String>,
    progress_percent: Option<u8>,
    results: Vec<ReportedResult>,
}

impl ProgressReport {
    /// Read the envelope, returning `None` for anything this build cannot act on.
    ///
    /// The contract is the parser: a report with a missing or invalid required field cannot be
    /// applied, and guessing would corrupt the projection.
    fn read(payload: &serde_json::Value) -> Option<Self> {
        let body = payload.get("payload").unwrap_or(payload);
        let report = serde_json::from_value::<OperationReported>(body.clone()).ok()?;

        Some(Self {
            operation_id: report.operation_id.0,
            status: report.status,
            stage: report.stage.map(String::from),
            progress_percent: report.progress_percent.map(u8::from),
            results: report
                .results
                .into_iter()
                .map(|result| ReportedResult {
                    result_kind: String::from(result.result_kind),
                    target: String::from(result.target),
                })
                .collect(),
        })
    }
}
