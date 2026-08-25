//! Listing one owner's operations, newest accepted first.
//!
//! The public operational views need a page of work, not one identifier at a time. This module
//! answers with rows only — identification and lifecycle facts straight off the composite index —
//! because the heavy payload collections stay the singular endpoint's job. Pagination is a keyset:
//! the caller hands back the last row's `(accepted_at, operation_id)` pair as an exclusive anchor,
//! so concurrent inserts can never shift what an in-flight walk has already seen or will still
//! see. No offset arithmetic exists anywhere on this path.

use platform_persistence::PersistenceError;
use ratatoskr_operation_contracts::OperationStatus;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::{Operation, OperationError};

/// Which operations to list, for whom, and how many.
///
/// `before` is the exclusive continuation anchor: the `(accepted_at, operation_id)` of the last
/// row of the previous page. Absent on a first page.
#[derive(Debug, Clone)]
pub struct ListScope<'a> {
    /// Whose operations. Every page is scoped to one owner; there is no cross-owner listing.
    pub owner_user_id: Uuid,
    /// Restrict to one lifecycle status, when given.
    pub status: Option<OperationStatus>,
    /// Restrict to one exact kind, when given.
    pub kind: Option<&'a str>,
    /// The exclusive keyset anchor of a continuation page.
    pub before: Option<(jiff::Timestamp, Uuid)>,
    /// At most this many rows are returned.
    pub limit: i64,
}

/// One page of rows, and whether the owner has more beyond it.
#[derive(Debug, Clone)]
pub struct Page {
    /// The rows, newest accepted first.
    pub rows: Vec<Operation>,
    /// Whether another page exists after this one. Decided by asking for one more row than the
    /// limit, so the caller never issues an empty follow-up page to find out.
    pub has_more: bool,
}

/// Read one page of an owner's operations.
///
/// The query walks the composite ownership index in its own order — newest accepted first,
/// operation identifier breaking ties — and stops at the exclusive anchor when one is given.
/// One row more than the limit is fetched, which is how `has_more` is decided without a count.
///
/// # Errors
///
/// [`OperationError::Persistence`] if the statement fails.
pub async fn list_operations<'e, E>(
    executor: E,
    scope: ListScope<'_>,
) -> Result<Page, OperationError>
where
    E: PgExecutor<'e>,
{
    // A negative or unrepresentable limit cannot occur through the public route, which bounds and
    // validates it; treated here as an empty page rather than a panic inside a query path.
    let limit = usize::try_from(scope.limit.max(0)).unwrap_or(0);
    let rows = sqlx::query(
        "select operation_id, owner_user_id, kind, status, stage, progress_percent,
                correlation_id, retryable, cancellation_requested_at,
                accepted_at, status_changed_at, terminated_at
           from operations.operations
          where owner_user_id = $1
            and ($2::text is null or status = $2)
            and ($3::text is null or kind = $3)
            and ($4::timestamptz is null
                 or (accepted_at, operation_id) < ($4, $5))
          order by accepted_at desc, operation_id desc
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

    let mut page = Page {
        rows: Vec::with_capacity(rows.len()),
        has_more,
    };
    for row in &rows {
        page.rows.push(crate::cancel::operation_from_row(row)?);
    }
    Ok(page)
}
