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
    pub action: String,
    /// What kind of thing was acted on.
    pub target_kind: String,
    /// Which one, when it has a UUID identity.
    pub target_id: Option<Uuid>,
    /// The decision.
    pub outcome: AuditOutcome,
    /// The namespaced correlation identifier the client also saw.
    pub correlation_id: String,
}

/// Append an audit record.
///
/// Takes an executor rather than a pool so it can join the caller's transaction: an audited action
/// and its record must commit together, or a denied action can be committed with no trace of the
/// denial.
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
    .bind(&event.action)
    .bind(&event.target_kind)
    .bind(event.target_id)
    .bind(event.outcome.as_str())
    .bind(&event.correlation_id)
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(())
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
