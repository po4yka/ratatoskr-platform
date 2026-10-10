//! The transaction every command-emitting route shares.
//!
//! A route that accepts work durably does five things in ONE transaction: it reserves the
//! idempotency key, creates the operation, enqueues the command, completes the reservation and
//! writes the audit record. A crash at any point leaves all five or none, which is what makes a
//! retried request safe. The routes differ in the operation they create and the command they
//! enqueue, and nothing else, so those two are the parameters.

use axum::response::Response;
use platform_core::FailureKind;
use platform_eventing::{Outbox, Subject};
use platform_idempotency::{Digest, Outcome};
use platform_identity::audit::{self, AuditEvent, AuditOutcome};
use uuid::Uuid;

use crate::{ApiState, Principal};

/// What a command-emitting route knows about its own request.
pub(crate) struct Intake<'a> {
    /// The route string the idempotency ledger keys on.
    pub(crate) route: &'static str,
    /// The kind of operation this route creates.
    pub(crate) kind: &'static str,
    /// The audit action recorded when the request is accepted.
    pub(crate) audit_action: &'static str,
    /// The caller's `Idempotency-Key`.
    pub(crate) key: &'a str,
    /// The exact bytes the request is fingerprinted over: everything that makes two requests the
    /// same or different (the body, and any path member that selects what the body acts on).
    pub(crate) fingerprint: &'a [u8],
    /// The request correlation (ADR-0007).
    pub(crate) correlation: &'a str,
}

/// The command one accepted request produces, built once its operation exists.
pub(crate) struct Prepared {
    pub(crate) subject: Subject,
    /// The envelope's `command_id`, which is also the outbox row id and the `Nats-Msg-Id`.
    pub(crate) command_id: Uuid,
    pub(crate) payload: serde_json::Value,
}

/// Reserve, create, enqueue, complete — in one transaction, so a crash at any point leaves all four
/// or none.
///
/// Shared by every command route along the boundary that means something: everything before it
/// decides whether this request is one we accept, `prepare` decides which command it becomes, and
/// this does the work.
pub(crate) async fn accept<F>(
    state: &ApiState,
    principal: Principal,
    intake: &Intake<'_>,
    answer: fn(Uuid) -> Response,
    prepare: F,
) -> Response
where
    F: FnOnce(&platform_operations::Operation, jiff::Timestamp) -> Result<Prepared, FailureKind>,
{
    let now = jiff::Timestamp::now();
    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let reservation = match platform_idempotency::reserve(
        &mut transaction,
        principal.user_id,
        intake.route,
        intake.kind,
        Digest::of_key(intake.key),
        Digest::of_body(intake.fingerprint),
        now,
        state.idempotency_ttl,
    )
    .await
    {
        Ok(reservation) => reservation,
        Err(error) => {
            tracing::error!(%error, "the idempotency key could not be reserved");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let record_id = match reservation.outcome() {
        Outcome::Proceed(record_id) => record_id,
        // S8.1: "Retrying the same payload returns the original operation."
        Outcome::Replay(operation_id) => return answer(operation_id),
        Outcome::Refuse => return platform_http::reject(FailureKind::IdempotencyConflict),
    };

    let operation = match platform_operations::accept(
        &mut *transaction,
        principal.user_id,
        intake.kind,
        intake.correlation,
        Some(intake.key),
        now,
    )
    .await
    {
        Ok(operation) => operation,
        Err(error) => {
            tracing::error!(%error, "the operation could not be accepted");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let prepared = match prepare(&operation, now) {
        Ok(prepared) => prepared,
        Err(kind) => return platform_http::reject(kind),
    };

    if let Err(error) = Outbox::enqueue(
        &mut *transaction,
        prepared.command_id,
        &prepared.subject,
        &prepared.payload,
        Some(operation.operation_id),
        now,
    )
    .await
    {
        tracing::error!(%error, "the command could not be enqueued");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    if let Err(error) = platform_idempotency::complete(
        &mut *transaction,
        record_id,
        Some(operation.operation_id),
        202,
        now,
    )
    .await
    {
        tracing::error!(%error, "the idempotency record could not be completed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    // Inside the transaction, so the record and the thing it records commit together. An audited
    // action with no record, or a record for an action that rolled back, are both worse than
    // neither.
    let event = submission(
        intake.audit_action,
        &principal,
        operation.operation_id,
        intake.correlation,
    );
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "the capture could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    if let Err(error) = transaction.commit().await {
        // Nothing happened: no reservation, no operation, no command. The client may retry with the
        // same key and get a clean first attempt.
        tracing::error!(%error, "the capture transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    answer(operation.operation_id)
}

/// The audit record of one accepted request.
///
/// Only the ALLOWED case exists, and that is a decision rather than an omission. These routes have
/// no authorization step beyond authentication, so there is no denial to record; a request that
/// fails authentication never reaches [`accept`], and an anonymous 401 has no actor to attribute: it
/// is counted by `http_server_request_duration_seconds{status}` and deliberately not written to a
/// table an unauthenticated caller could grow one row at a time.
fn submission(
    action: &'static str,
    principal: &Principal,
    operation_id: Uuid,
    correlation: &str,
) -> AuditEvent {
    AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action,
        target_kind: "operation",
        target_id: Some(operation_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.to_owned(),
    }
}
