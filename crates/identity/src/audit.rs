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
