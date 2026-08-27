//! Deployment-wide schedule status inspection for an authorized Platform owner.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::SchedulingError;

/// Keyset anchor and bound for one schedule-status page.
#[derive(Debug, Clone, Copy)]
pub struct AdminListScope {
    /// Exclusive ascending due-time anchor.
    pub after: Option<(jiff::Timestamp, Uuid)>,
    /// Maximum number of rows returned.
    pub limit: i64,
}

/// One redacted row from `operations.schedule_status`.
#[derive(Debug, Clone)]
pub struct InspectedSchedule {
    /// Stable schedule identity.
    pub schedule_id: Uuid,
    /// Stable service label.
    pub service_name: String,
    /// Stable schedule name within the service.
    pub name: String,
    /// User whose work the schedule produces.
    pub owner_user_id: Uuid,
    /// Next due instant.
    pub next_due_at: jiff::Timestamp,
    /// Whether future occurrences are enabled.
    pub enabled: bool,
    /// Stored operation status for the latest occurrence, when any.
    pub last_outcome: Option<String>,
}

/// One bounded ascending page of schedule status rows.
#[derive(Debug, Clone)]
pub struct AdminPage {
    /// Rows ordered by next due instant then identifier.
    pub rows: Vec<InspectedSchedule>,
    /// Whether another page exists after the last returned row.
    pub has_more: bool,
}

/// Read schedule status without selecting command payload or schedule configuration columns.
///
/// # Errors
///
/// [`SchedulingError::Persistence`] if the statement or a stored value cannot be read.
pub async fn list_admin_schedules<'e, E>(
    executor: E,
    scope: AdminListScope,
) -> Result<AdminPage, SchedulingError>
where
    E: PgExecutor<'e>,
{
    let limit = usize::try_from(scope.limit.max(0)).unwrap_or(0);
    let rows = sqlx::query(
        "select schedule_id, service_name, name, owner_user_id,
                next_due_at, enabled, last_outcome
           from operations.schedule_status
          where ($1::timestamptz is null
                 or (next_due_at, schedule_id) > ($1, $2))
          order by next_due_at, schedule_id
          limit $3",
    )
    .bind(
        scope
            .after
            .as_ref()
            .map(|(next_due_at, _)| crate::to_offset(*next_due_at)),
    )
    .bind(scope.after.as_ref().map(|(_, schedule_id)| *schedule_id))
    .bind(i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1))
    .fetch_all(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let has_more = rows.len() > limit;
    let mut rows = rows;
    rows.truncate(limit);
    let mut inspected = Vec::with_capacity(rows.len());
    for row in rows {
        let next_due_at: time::OffsetDateTime = row
            .try_get("next_due_at")
            .map_err(PersistenceError::Query)?;
        inspected.push(InspectedSchedule {
            schedule_id: row
                .try_get("schedule_id")
                .map_err(PersistenceError::Query)?,
            service_name: row
                .try_get("service_name")
                .map_err(PersistenceError::Query)?,
            name: row.try_get("name").map_err(PersistenceError::Query)?,
            owner_user_id: row
                .try_get("owner_user_id")
                .map_err(PersistenceError::Query)?,
            next_due_at: crate::from_offset(next_due_at),
            enabled: row.try_get("enabled").map_err(PersistenceError::Query)?,
            last_outcome: row
                .try_get("last_outcome")
                .map_err(PersistenceError::Query)?,
        });
    }

    Ok(AdminPage {
        rows: inspected,
        has_more,
    })
}
