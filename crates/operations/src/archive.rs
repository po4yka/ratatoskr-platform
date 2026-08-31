//! The immutable Edge receipt binding for one AI archive operation.

use platform_persistence::PersistenceError;
use ratatoskr_error_contracts::ErrorEnvelope;
use ratatoskr_operation_contracts::OperationStatus;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::transition::Transition;
use crate::{OperationError, record_error, record_status, to_offset};

/// Edge's immutable binding for one accepted raw AI archive receipt.
///
/// This is request metadata only. The receiving provider service owns archive bytes, parsing and
/// completeness; Edge keeps the binding so an authenticated device cannot substitute a different
/// provider or digest after it has received a pollable operation identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiArchiveAcceptance {
    /// The public operation that owns this upload.
    pub operation_id: Uuid,
    /// The user that may deliver the archive.
    pub owner_user_id: Uuid,
    /// The export-agent device that prepared this operation.
    pub device_id: Uuid,
    /// The bounded provider routing token.
    pub provider: String,
    /// Lowercase SHA-256 of the exact archive bytes.
    pub sha256: String,
    /// Declared number of bytes.
    pub byte_size: i64,
    /// When Edge durably accepted the binding.
    pub accepted_at: jiff::Timestamp,
}

/// Persist the immutable metadata that binds an archive delivery to its operation.
///
/// Call this in the same transaction that created the operation and completed its idempotency
/// reservation. The schema is the final authority for provider, digest and size bounds.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the binding cannot be written.
pub async fn record_ai_archive_acceptance<'e, E>(
    executor: E,
    acceptance: &AiArchiveAcceptance,
) -> Result<(), OperationError>
where
    E: PgExecutor<'e>,
{
    sqlx::query(
        "insert into operations.ai_archive_acceptances
             (operation_id, owner_user_id, device_id, provider, sha256, byte_size, accepted_at)
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(acceptance.operation_id)
    .bind(acceptance.owner_user_id)
    .bind(acceptance.device_id)
    .bind(&acceptance.provider)
    .bind(&acceptance.sha256)
    .bind(acceptance.byte_size)
    .bind(to_offset(acceptance.accepted_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(())
}

/// Read the delivery binding for a prepared archive operation.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the statement fails.
pub async fn find_ai_archive_acceptance<'e, E>(
    executor: E,
    operation_id: Uuid,
) -> Result<Option<AiArchiveAcceptance>, OperationError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select operation_id, owner_user_id, device_id, provider, sha256, byte_size, accepted_at
           from operations.ai_archive_acceptances where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| {
        Ok(AiArchiveAcceptance {
            operation_id: row
                .try_get("operation_id")
                .map_err(PersistenceError::Query)?,
            owner_user_id: row
                .try_get("owner_user_id")
                .map_err(PersistenceError::Query)?,
            device_id: row.try_get("device_id").map_err(PersistenceError::Query)?,
            provider: row.try_get("provider").map_err(PersistenceError::Query)?,
            sha256: row.try_get("sha256").map_err(PersistenceError::Query)?,
            byte_size: row.try_get("byte_size").map_err(PersistenceError::Query)?,
            accepted_at: crate::from_offset(
                row.try_get("accepted_at")
                    .map_err(PersistenceError::Query)?,
            ),
        })
    })
    .transpose()
}

/// Terminate an archive operation when Edge observed that the configured importer refused delivery.
///
/// The diagnostic is deliberately generic: the importer response may contain provider details,
/// paths or archive content and none belong in Platform's operation projection.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the terminal transition or its diagnostic cannot be stored.
pub async fn fail_ai_archive_delivery(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    now: jiff::Timestamp,
) -> Result<(), OperationError> {
    const CODE: &str = "platform.ai_archive.delivery_failed";
    const MESSAGE: &str = "Archive delivery to the importer failed.";

    let mut transaction = pool.begin().await.map_err(PersistenceError::Query)?;
    let (transition, _) = record_status(
        &mut transaction,
        operation_id,
        OperationStatus::Failed,
        Some("delivery"),
        None,
        Some(MESSAGE),
        now,
    )
    .await?;
    if matches!(transition, Transition::Advance(_)) {
        let error = ErrorEnvelope::new(
            ratatoskr_error_contracts::ErrorCode::parse(CODE)
                .map_err(|error| OperationError::ContractViolation(error.to_string()))?,
            ratatoskr_identifiers::SafeMessage::parse(MESSAGE)
                .map_err(|error| OperationError::ContractViolation(error.to_string()))?,
            true,
        );
        record_error(&mut transaction, operation_id, &error, now).await?;
    }
    transaction
        .commit()
        .await
        .map_err(PersistenceError::Query)?;
    Ok(())
}
