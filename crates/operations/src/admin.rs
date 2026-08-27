//! Deployment-wide operation inspection for an authorized Platform owner.

use platform_persistence::PersistenceError;
use ratatoskr_operation_contracts::OperationStatus;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{Operation, OperationError};

/// Bounded filters and keyset anchor for an owner inspection page.
#[derive(Debug, Clone)]
pub struct AdminListScope<'a> {
    /// Restrict to one operation owner.
    pub owner_user_id: Option<Uuid>,
    /// Restrict to one lifecycle status.
    pub status: Option<OperationStatus>,
    /// Restrict to one exact operation kind.
    pub kind: Option<&'a str>,
    /// Exclusive newest-first keyset anchor.
    pub before: Option<(jiff::Timestamp, Uuid)>,
    /// Maximum number of rows returned.
    pub limit: i64,
}

/// One stored operation and its latest safe terminal error code.
#[derive(Debug, Clone)]
pub struct InspectedOperation {
    /// Lifecycle row.
    pub operation: Operation,
    /// Stable safe error code only; no message, payload, or diagnostic.
    pub failure_code: Option<String>,
}

/// One bounded page of deployment-wide operations.
#[derive(Debug, Clone)]
pub struct AdminPage {
    /// Rows ordered by acceptance instant then identifier, newest first.
    pub rows: Vec<InspectedOperation>,
    /// Whether another page exists after the last returned row.
    pub has_more: bool,
}

/// Read one page across users without loading diagnostic payloads.
///
/// # Errors
///
/// `OperationError::Persistence` if the statement fails, or
/// `OperationError::ContractViolation` if a stored lifecycle row is invalid.
pub async fn list_admin_operations<'e, E>(
    executor: E,
    scope: AdminListScope<'_>,
) -> Result<AdminPage, OperationError>
where
    E: PgExecutor<'e>,
{
    let limit = usize::try_from(scope.limit.max(0)).unwrap_or(0);
    let rows = sqlx::query(
        "select operation.operation_id, operation.owner_user_id, operation.kind,
                operation.status, operation.stage, operation.progress_percent,
                operation.correlation_id, operation.retryable,
                operation.cancellation_requested_at, operation.accepted_at,
                operation.status_changed_at, operation.terminated_at,
                failure.code as failure_code
           from operations.operations operation
           left join lateral (
               select error.code
                 from operations.operation_errors error
                where error.operation_id = operation.operation_id
                  and error.severity = 'error'
                order by error.recorded_at desc, error.error_id desc
                limit 1
           ) failure on true
          where ($1::uuid is null or operation.owner_user_id = $1)
            and ($2::text is null or operation.status = $2)
            and ($3::text is null or operation.kind = $3)
            and ($4::timestamptz is null
                 or (operation.accepted_at, operation.operation_id) < ($4, $5))
          order by operation.accepted_at desc, operation.operation_id desc
          limit $6",
    )
    .bind(scope.owner_user_id)
    .bind(scope.status.map(crate::status_str))
    .bind(scope.kind)
    .bind(
        scope
            .before
            .as_ref()
            .map(|(accepted_at, _)| crate::to_offset(*accepted_at)),
    )
    .bind(scope.before.as_ref().map(|(_, operation_id)| *operation_id))
    .bind(i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1))
    .fetch_all(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let has_more = rows.len() > limit;
    let mut rows = rows;
    rows.truncate(limit);
    let mut inspected = Vec::with_capacity(rows.len());
    for row in &rows {
        inspected.push(InspectedOperation {
            operation: crate::cancel::operation_from_row(row)?,
            failure_code: row
                .try_get("failure_code")
                .map_err(PersistenceError::Query)?,
        });
    }

    Ok(AdminPage {
        rows: inspected,
        has_more,
    })
}
