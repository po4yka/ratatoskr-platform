//! The end of a notes operation whose forward Edge observed fail.

use platform_persistence::PersistenceError;
use ratatoskr_error_contracts::{ErrorCode, ErrorEnvelope};
use ratatoskr_operation_contracts::OperationStatus;
use uuid::Uuid;

use crate::transition::Transition;
use crate::{OperationError, record_error, record_status};

/// Terminate a notes operation as `failed` when Edge's forward of its request to the notes service
/// failed: the service was not reachable, did not answer in time, refused the request, or gave an
/// answer that Edge cannot use. Edge forwards the connect (`notes.connection.connect`) and the
/// write-mode switch (`notes.connection.write_mode`).
///
/// `stage` names the request, and `message` is the fixed message that the progress history and the
/// error record keep. The answer of the notes service can name the vault or the account, so no part
/// of it is kept: only its stable `code`, the code of the Edge fault or of the error envelope of the
/// notes service. The status goes through the one transition applier, so a report of the notes
/// service that arrived first is not overwritten.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the terminal transition or its diagnostic cannot be stored,
/// [`OperationError::ConflictingOutcome`] if the notes service already reported another terminal
/// status, [`OperationError::ContractViolation`] if `message` is not a safe message.
pub async fn fail_notes_forward(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    stage: &str,
    message: &str,
    code: &ErrorCode,
    retryable: bool,
    now: jiff::Timestamp,
) -> Result<(), OperationError> {
    let mut transaction = pool.begin().await.map_err(PersistenceError::Query)?;
    let (transition, _) = record_status(
        &mut transaction,
        operation_id,
        OperationStatus::Failed,
        Some(stage),
        None,
        Some(message),
        now,
    )
    .await?;
    if matches!(transition, Transition::Advance(_)) {
        let error = ErrorEnvelope::new(
            code.clone(),
            ratatoskr_identifiers::SafeMessage::parse(message)
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
