//! The transaction every command-emitting route shares.
//!
//! A route that accepts work durably does five things in ONE transaction: it reserves the
//! idempotency key, creates the operation, enqueues the command, completes the reservation and
//! writes the audit record. A crash at any point leaves all five or none, which is what makes a
//! retried request safe. The routes differ in the operation they create and the command they
//! enqueue, and nothing else, so those two are the parameters.
//!
//! A route whose work Edge forwards itself (the notes write-mode switch) takes the same transaction
//! without the command, through [`mint`].

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
    /// The one service the command is dispatched to, recorded on the operation as the only
    /// producer allowed to report for it (XR-021 R3-02).
    pub(crate) reporter: Option<&'static str>,
}

/// The command one accepted request produces, built once its operation exists.
pub(crate) struct Prepared {
    pub(crate) subject: Subject,
    /// The envelope's `command_id`, which is also the outbox row id and the `Nats-Msg-Id`.
    pub(crate) command_id: Uuid,
    pub(crate) payload: serde_json::Value,
}

/// The edit of a site owner session (SITE-027): a row of `operations.site_edits` that commits in
/// the transaction of its operation and its command, so that no command exists whose success or
/// conflict cannot reach the site.
pub(crate) struct SiteEditRow<'a> {
    pub(crate) site_id: &'a str,
    pub(crate) source_ref: &'a str,
    /// The content hash of the edit: the note's revision when the operation succeeds.
    pub(crate) revision: &'a str,
}

/// What [`mint`] did with one request.
pub(crate) enum Minted {
    /// A new operation exists, and the caller does the work that follows it.
    Fresh(Uuid),
    /// The key and the request were seen before: this is that operation, and nothing was done.
    Replayed(Uuid),
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
    match admit(state, principal, intake, None, |operation, now| {
        prepare(operation, now).map(Some)
    })
    .await
    {
        Ok(Minted::Fresh(operation_id) | Minted::Replayed(operation_id)) => answer(operation_id),
        Err(kind) => platform_http::reject(kind),
    }
}

/// Reserve, create, complete — in one transaction, with no command: for a route whose work Edge
/// forwards itself once the operation exists.
pub(crate) async fn mint(
    state: &ApiState,
    principal: Principal,
    intake: &Intake<'_>,
) -> Result<Minted, FailureKind> {
    admit(state, principal, intake, None, |_, _| Ok(None)).await
}

/// The one transaction of [`accept`] and [`mint`]. `prepare` returns the command to enqueue, or
/// `None` when the route enqueues none; `site_edit` is the row of a site edit, written with them.
/// `Err` is the refusal to answer.
pub(crate) async fn admit<F>(
    state: &ApiState,
    principal: Principal,
    intake: &Intake<'_>,
    site_edit: Option<&SiteEditRow<'_>>,
    prepare: F,
) -> Result<Minted, FailureKind>
where
    F: FnOnce(
        &platform_operations::Operation,
        jiff::Timestamp,
    ) -> Result<Option<Prepared>, FailureKind>,
{
    let now = jiff::Timestamp::now();
    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return Err(FailureKind::RequestTimeout);
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
            return Err(FailureKind::RequestTimeout);
        }
    };

    let record_id = match reservation.outcome() {
        Outcome::Proceed(record_id) => record_id,
        // S8.1: "Retrying the same payload returns the original operation."
        Outcome::Replay(operation_id) => return Ok(Minted::Replayed(operation_id)),
        Outcome::Refuse => return Err(FailureKind::IdempotencyConflict),
    };

    let operation = match platform_operations::accept(
        &mut *transaction,
        principal.user_id,
        intake.kind,
        intake.correlation,
        Some(intake.key),
        intake.reporter,
        now,
    )
    .await
    {
        Ok(operation) => operation,
        Err(error) => {
            tracing::error!(%error, "the operation could not be accepted");
            return Err(FailureKind::RequestTimeout);
        }
    };

    let prepared = prepare(&operation, now)?;

    if let Some(prepared) = prepared
        && let Err(error) = Outbox::enqueue(
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
        return Err(FailureKind::RequestTimeout);
    }

    if let Some(row) = site_edit {
        record(&mut transaction, operation.operation_id, row).await?;
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
        return Err(FailureKind::RequestTimeout);
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
        return Err(FailureKind::RequestTimeout);
    }

    if let Err(error) = transaction.commit().await {
        // Nothing happened: no reservation, no operation, no command. The client may retry with the
        // same key and get a clean first attempt.
        tracing::error!(%error, "the capture transaction could not be committed");
        return Err(FailureKind::RequestTimeout);
    }

    Ok(Minted::Fresh(operation.operation_id))
}

/// Write the row of a site edit in the transaction of its operation.
async fn record(
    transaction: &mut sqlx::PgTransaction<'_>,
    operation_id: Uuid,
    row: &SiteEditRow<'_>,
) -> Result<(), FailureKind> {
    sqlx::query(
        "insert into operations.site_edits (operation_id, site_id, source_ref, revision, created_at)
         values ($1, $2, $3, $4, now())",
    )
    .bind(operation_id)
    .bind(row.site_id)
    .bind(row.source_ref)
    .bind(row.revision)
    .execute(&mut **transaction)
    .await
    .map(drop)
    .map_err(|error| {
        tracing::error!(%error, "the site edit could not be recorded");
        FailureKind::RequestTimeout
    })
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
