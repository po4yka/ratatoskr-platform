//! The public-action audit trail.
//!
//! `ARCHITECTURE.md` S15: audit records capture actor, action, target and result without copying
//! sensitive content. There is deliberately no payload field on [`AuditEvent`] — a free-form blob
//! is how private content reaches an audit export — and the correlation identifier is stored in the
//! same namespaced form the client saw, so a support conversation joins to the trail on one string.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::to_offset;

/// What the platform decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// The action proceeded.
    Allowed,
    /// Authorization refused it.
    Denied,
    /// It was permitted but did not complete.
    Failed,
}

impl AuditOutcome {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }
}

/// One audited public action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// The record's identity.
    pub audit_event_id: Uuid,
    /// The acting user, when there was one. Absent for an unauthenticated attempt, which is
    /// exactly the case worth auditing.
    pub actor_user_id: Option<Uuid>,
    /// The session that acted, when there was one.
    pub actor_session_id: Option<Uuid>,
    /// A dotted action name, e.g. `session.create`.
    ///
    /// `&'static str` and not `String`, because this value is also a metric label
    /// (`platform_auth_decisions_total`). A label built from anything a request carries is a
    /// cardinality bomb, and the type is what makes that impossible rather than a rule somebody has
    /// to remember — the same device `platform_core::config::Violation` uses to keep supplied values
    /// out of a failure report.
    pub action: &'static str,
    /// What kind of thing was acted on. `&'static str` for the same reason as `action`.
    pub target_kind: &'static str,
    /// Which one, when it has a UUID identity.
    pub target_id: Option<Uuid>,
    /// The decision.
    pub outcome: AuditOutcome,
    /// The namespaced correlation identifier the client also saw.
    pub correlation_id: String,
}

/// Keyset anchor and bound for one deployment-wide audit page.
#[derive(Debug, Clone, Copy)]
pub struct AdminListScope {
    /// Exclusive newest-first occurrence anchor.
    pub before: Option<(jiff::Timestamp, Uuid)>,
    /// Maximum number of rows returned.
    pub limit: i64,
}

/// One stored audit record containing only the fixed audit columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedAuditEvent {
    /// Stable record identity.
    pub audit_event_id: Uuid,
    /// Platform-observed occurrence instant.
    pub occurred_at: jiff::Timestamp,
    /// Acting user, when one existed.
    pub actor_user_id: Option<Uuid>,
    /// Acting session, when one existed.
    pub actor_session_id: Option<Uuid>,
    /// Stable audited action token.
    pub action: String,
    /// Stable audited target kind.
    pub target_kind: String,
    /// Target identity, when one existed.
    pub target_id: Option<Uuid>,
    /// Stored decision token.
    pub outcome: String,
    /// Namespaced correlation reference.
    pub correlation_id: String,
}

/// One bounded newest-first audit page.
#[derive(Debug, Clone)]
pub struct AdminPage {
    /// Rows ordered by occurrence instant then identifier, newest first.
    pub rows: Vec<InspectedAuditEvent>,
    /// Whether another page exists after the last returned row.
    pub has_more: bool,
}

/// Append an audit record, and count the decision.
///
/// Takes an executor rather than a pool so it can join the caller's transaction: an audited action
/// and its record must commit together, or a denied action can be committed with no trace of the
/// denial.
///
/// The counter — `platform_auth_decisions_total`, `ARCHITECTURE.md` S16 item 2 — is incremented
/// here and nowhere else, so the series and the table cannot disagree about what was decided.
/// It carries the action and the outcome and no identifier of any kind: S16 asks for these outcomes
/// "without sensitive identifiers", and a user id in a label is a disclosure as well as unbounded
/// cardinality.
///
/// It is incremented AFTER the insert succeeds. A record that failed to commit is not a decision
/// anybody can audit, and counting it would put a number on a dashboard with nothing behind it.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails, including when `correlation_id` is not in the
/// namespaced form the schema requires.
pub async fn record<'e, E>(
    executor: E,
    event: &AuditEvent,
    occurred_at: jiff::Timestamp,
) -> Result<(), PersistenceError>
where
    E: PgExecutor<'e>,
{
    sqlx::query(
        "insert into identity.audit_events
             (audit_event_id, occurred_at, actor_user_id, actor_session_id,
              action, target_kind, target_id, outcome, correlation_id)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(event.audit_event_id)
    .bind(to_offset(occurred_at))
    .bind(event.actor_user_id)
    .bind(event.actor_session_id)
    .bind(event.action)
    .bind(event.target_kind)
    .bind(event.target_id)
    .bind(event.outcome.as_str())
    .bind(&event.correlation_id)
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    metrics::counter!(
        platform_telemetry::metrics::PLATFORM_AUTH_DECISIONS_TOTAL,
        "action" => event.action,
        "outcome" => event.outcome.as_str(),
    )
    .increment(1);

    Ok(())
}

/// Read a bounded audit page without joining to sessions, credentials, or request content.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement or a stored value cannot be read.
pub async fn list_admin_events<'e, E>(
    executor: E,
    scope: AdminListScope,
) -> Result<AdminPage, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let limit = usize::try_from(scope.limit.max(0)).unwrap_or(0);
    let rows = sqlx::query(
        "select audit_event_id, occurred_at, actor_user_id, actor_session_id,
                action, target_kind, target_id, outcome, correlation_id
           from identity.audit_events
          where ($1::timestamptz is null
                 or (occurred_at, audit_event_id) < ($1, $2))
          order by occurred_at desc, audit_event_id desc
          limit $3",
    )
    .bind(
        scope
            .before
            .as_ref()
            .map(|(occurred_at, _)| to_offset(*occurred_at)),
    )
    .bind(scope.before.as_ref().map(|(_, event_id)| *event_id))
    .bind(i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1))
    .fetch_all(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let has_more = rows.len() > limit;
    let mut rows = rows;
    rows.truncate(limit);
    let mut inspected = Vec::with_capacity(rows.len());
    for row in rows {
        let occurred_at: time::OffsetDateTime = row
            .try_get("occurred_at")
            .map_err(PersistenceError::Query)?;
        inspected.push(InspectedAuditEvent {
            audit_event_id: row
                .try_get("audit_event_id")
                .map_err(PersistenceError::Query)?,
            occurred_at: crate::from_offset(occurred_at),
            actor_user_id: row
                .try_get("actor_user_id")
                .map_err(PersistenceError::Query)?,
            actor_session_id: row
                .try_get("actor_session_id")
                .map_err(PersistenceError::Query)?,
            action: row.try_get("action").map_err(PersistenceError::Query)?,
            target_kind: row
                .try_get("target_kind")
                .map_err(PersistenceError::Query)?,
            target_id: row.try_get("target_id").map_err(PersistenceError::Query)?,
            outcome: row.try_get("outcome").map_err(PersistenceError::Query)?,
            correlation_id: row
                .try_get("correlation_id")
                .map_err(PersistenceError::Query)?,
        });
    }

    Ok(AdminPage {
        rows: inspected,
        has_more,
    })
}

/// Delete audit records older than `before`, at most `limit` of them.
///
/// The longest window of the four by an order of magnitude, and the one whose length is a policy
/// rather than a mechanism: everything else here is deleted once it can no longer affect
/// correctness, and this is deleted when somebody decides how long an incident may go unnoticed.
///
/// Bounded per call, so a table nobody has pruned drains over hours rather than taking one long
/// lock.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn collect_before<'e, E>(
    executor: E,
    before: jiff::Timestamp,
    limit: i64,
) -> Result<u64, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let done = sqlx::query(
        "delete from identity.audit_events
          where audit_event_id in (
              select audit_event_id from identity.audit_events
               where occurred_at < $1
               limit $2
          )",
    )
    .bind(to_offset(before))
    .bind(limit)
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(done.rows_affected())
}

/// Count the audit records carrying one correlation identifier.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn count_by_correlation<'e, E>(
    executor: E,
    correlation_id: &str,
) -> Result<i64, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select count(*) as count from identity.audit_events where correlation_id = $1",
    )
    .bind(correlation_id)
    .fetch_one(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.try_get::<i64, _>("count")
        .map_err(PersistenceError::Query)
}
