//! Domain-service schedule registration.

use std::collections::BTreeSet;
use std::str::FromStr as _;

use chrono::DateTime;
use cron::Schedule;
use jiff::Timestamp;
use platform_eventing::inbox::Outcome;
use platform_eventing::{Handler, Incoming, MessageClass, Subject};
use platform_identity::{AuditEvent, AuditOutcome};
use platform_persistence::PersistenceError;
use sqlx::PgPool;
use uuid::Uuid;

use crate::SchedulingError;

/// The command type domains use to register recurring work.
pub const REGISTRATION_COMMAND_TYPE: &str = "platform.schedule.registration_requested.v1";

/// The decision made about a registration payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleRegistration {
    /// The registration was stored.
    Applied,
    /// The registration was refused permanently.
    Rejected,
}

/// Validates and durably applies registrations from named domain services.
#[derive(Debug, Clone)]
pub struct RegistrationHandler {
    allowed: BTreeSet<String>,
}

impl RegistrationHandler {
    /// Build a handler with the deployment's explicitly allowed service producers.
    #[must_use]
    pub fn new(allowed: Vec<String>) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
        }
    }

    /// Apply one registration in a transaction.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulingError`] when the database or audit trail cannot commit the decision.
    pub async fn register(
        &self,
        pool: &PgPool,
        message: &Incoming,
        now: Timestamp,
    ) -> Result<ScheduleRegistration, SchedulingError> {
        let mut transaction = pool.begin().await.map_err(PersistenceError::Query)?;
        let outcome = self.apply(&mut transaction, message, now).await?;
        transaction
            .commit()
            .await
            .map_err(PersistenceError::Query)?;
        Ok(outcome)
    }

    async fn apply(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
        now: Timestamp,
    ) -> Result<ScheduleRegistration, SchedulingError> {
        let Some(input) = Registration::read(message) else {
            audit(transaction, None, None, AuditOutcome::Denied, message, now).await?;
            return Ok(ScheduleRegistration::Rejected);
        };
        if !self.allowed.contains(&message.producer)
            || message.producer != input.service_name
            || !is_label(&input.service_name)
            || !is_label(&input.name)
            || !is_operation_kind(&input.operation_kind)
            || next_after(&input.cron_expression, now).is_none()
            || Subject::new(MessageClass::Command, &input.command_type).is_err()
            || !input.payload.is_object()
        {
            audit(transaction, None, None, AuditOutcome::Denied, message, now).await?;
            return Ok(ScheduleRegistration::Rejected);
        }
        let Some(next_due_at) = next_after(&input.cron_expression, now) else {
            return Ok(ScheduleRegistration::Rejected);
        };
        let schedule_id: Uuid = sqlx::query_scalar(
            "insert into operations.schedules
                (schedule_id, service_name, name, owner_user_id, command_type, operation_kind,
                 payload, cron_expression, next_due_at, enabled, created_at, updated_at)
             values (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)
             on conflict (service_name, name) do update set
                owner_user_id = excluded.owner_user_id, command_type = excluded.command_type,
                operation_kind = excluded.operation_kind, payload = excluded.payload,
                cron_expression = excluded.cron_expression, enabled = excluded.enabled,
                next_due_at = case when operations.schedules.next_due_at <= $10
                                   then operations.schedules.next_due_at else excluded.next_due_at end,
                updated_at = excluded.updated_at
             returning schedule_id",
        )
        .bind(&input.service_name)
        .bind(&input.name)
        .bind(input.owner_user_id)
        .bind(&input.command_type)
        .bind(&input.operation_kind)
        .bind(&input.payload)
        .bind(&input.cron_expression)
        .bind(to_offset(next_due_at))
        .bind(input.enabled)
        .bind(to_offset(now))
        .fetch_one(&mut **transaction)
        .await
        .map_err(PersistenceError::Query)?;
        audit(
            transaction,
            Some(input.owner_user_id),
            Some(schedule_id),
            AuditOutcome::Allowed,
            message,
            now,
        )
        .await?;
        Ok(ScheduleRegistration::Applied)
    }
}

impl Handler for RegistrationHandler {
    async fn handle(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
    ) -> Result<Outcome, platform_eventing::EventingError> {
        match self.apply(transaction, message, Timestamp::now()).await {
            Ok(ScheduleRegistration::Applied) => Ok(Outcome::Applied),
            Ok(ScheduleRegistration::Rejected) => Ok(Outcome::Rejected),
            Err(error) => Err(platform_eventing::EventingError::Persistence(match error {
                SchedulingError::Persistence(error) => error,
                other => return Err(platform_eventing::EventingError::Bus(other.to_string())),
            })),
        }
    }
}

#[derive(Debug)]
struct Registration {
    service_name: String,
    name: String,
    owner_user_id: Uuid,
    cron_expression: String,
    command_type: String,
    operation_kind: String,
    payload: serde_json::Value,
    enabled: bool,
}

impl Registration {
    fn read(message: &Incoming) -> Option<Self> {
        if message.subject.as_str() != format!("cmd.{REGISTRATION_COMMAND_TYPE}") {
            return None;
        }
        let body = message.payload.get("payload")?;
        Some(Self {
            service_name: body.get("service_name")?.as_str()?.to_owned(),
            name: body.get("name")?.as_str()?.to_owned(),
            owner_user_id: body.get("owner_user_id")?.as_str()?.parse().ok()?,
            cron_expression: body.get("cron_expression")?.as_str()?.to_owned(),
            command_type: body.get("command_type")?.as_str()?.to_owned(),
            operation_kind: body.get("operation_kind")?.as_str()?.to_owned(),
            payload: body.get("payload")?.clone(),
            enabled: body.get("enabled")?.as_bool()?,
        })
    }
}

fn is_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes.iter().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase()
            } else {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(*byte, b'_' | b'-')
            }
        })
}

fn is_operation_kind(value: &str) -> bool {
    let segments: Vec<&str> = value.split('.').collect();
    (2..=4).contains(&segments.len())
        && segments.into_iter().all(|segment| {
            let bytes = segment.as_bytes();
            (1..=32).contains(&bytes.len())
                && bytes.iter().enumerate().all(|(index, byte)| {
                    if index == 0 {
                        byte.is_ascii_lowercase()
                    } else {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_'
                    }
                })
        })
}

/// First UTC five-field cron occurrence strictly after `after`.
pub(crate) fn next_after(expression: &str, after: Timestamp) -> Option<Timestamp> {
    if expression.split_whitespace().count() != 5 {
        return None;
    }
    let schedule = Schedule::from_str(&format!("0 {expression} *")).ok()?;
    let after = DateTime::from_timestamp(after.as_second(), 0)?;
    let next = schedule.after(&after).next()?;
    Timestamp::from_second(next.timestamp()).ok()
}

/// Deterministic identifier of the occurrence of `schedule_id` due at `due_at`.
///
/// `PostgreSQL` stores timestamps at microsecond precision, so the `UUIDv5` name uses the same
/// precision and survives an edit or a broker redelivery of the same due occurrence.
#[must_use]
pub fn occurrence_id(schedule_id: Uuid, due_at: Timestamp) -> Uuid {
    const OCCURRENCE_NAMESPACE: Uuid = Uuid::from_u128(0x8c1f_4a2e_6d73_4b90_9f21_5e0c_7a48_d3b6);
    let name = format!("{schedule_id}:{}", due_at.as_microsecond());
    Uuid::new_v5(&OCCURRENCE_NAMESPACE, name.as_bytes())
}

async fn audit(
    transaction: &mut sqlx::PgTransaction<'_>,
    owner_user_id: Option<Uuid>,
    schedule_id: Option<Uuid>,
    outcome: AuditOutcome,
    message: &Incoming,
    now: Timestamp,
) -> Result<(), SchedulingError> {
    let correlation_id = message
        .payload
        .get("correlation_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("correlation:unknown")
        .to_owned();
    platform_identity::audit::record(
        &mut **transaction,
        &AuditEvent {
            audit_event_id: Uuid::now_v7(),
            actor_user_id: owner_user_id,
            actor_session_id: None,
            action: "schedule.registration",
            target_kind: "schedule",
            target_id: schedule_id,
            outcome,
            correlation_id,
        },
        now,
    )
    .await
    .map_err(SchedulingError::Persistence)
}

fn to_offset(value: Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
