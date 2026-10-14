//! The end of a notes connect operation that Edge observed fail.

use platform_persistence::PersistenceError;
use ratatoskr_error_contracts::{ErrorCode, ErrorEnvelope};
use ratatoskr_operation_contracts::OperationStatus;
use uuid::Uuid;

use crate::transition::Transition;
use crate::{OperationError, record_error, record_status};

/// What the progress history and the error record say. The answer of the notes service can name the
/// vault or the account, so no part of it is kept: only its stable code.
const MESSAGE: &str = "The notes service did not accept the connection.";

/// Terminate a notes connect operation as `failed` when its forward to the notes service failed:
/// the service was not reachable, did not answer in time, refused the connect, or gave an answer
/// that Edge cannot use.
///
/// `code` is the stable code of the failure: the code of the Edge fault, or the code of the error
/// envelope of the notes service. The status goes through the one transition applier, so a report
/// of the notes service that arrived first is not overwritten.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the terminal transition or its diagnostic cannot be stored,
/// [`OperationError::ConflictingOutcome`] if the notes service already reported another terminal
/// status.
pub async fn fail_notes_connect(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    code: &ErrorCode,
    retryable: bool,
    now: jiff::Timestamp,
) -> Result<(), OperationError> {
    let mut transaction = pool.begin().await.map_err(PersistenceError::Query)?;
    let (transition, _) = record_status(
        &mut transaction,
        operation_id,
        OperationStatus::Failed,
        Some("connect"),
        None,
        Some(MESSAGE),
        now,
    )
    .await?;
    if matches!(transition, Transition::Advance(_)) {
        let error = ErrorEnvelope::new(
            code.clone(),
            ratatoskr_identifiers::SafeMessage::parse(MESSAGE)
                .map_err(|error| OperationError::ContractViolation(error.to_string()))?,
            retryable,
        );
        record_error(&mut transaction, operation_id, &error, now).await?;
    }
    transaction
        .commit()
        .await
        .map_err(PersistenceError::Query)?;
    Ok(())
}
