//! Submitting a capture.
//!
//! The one route that exercises the whole stack, and the reason milestones 2 to 4 were built the way
//! they were: the idempotency reservation, the operation record and the outbox row are written in
//! ONE transaction, so a crash at any point leaves all three or none.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use http::HeaderMap;
use platform_core::FailureKind;
use platform_eventing::{MessageClass, Outbox, Subject};
use platform_idempotency::{Digest, Reservation};
use uuid::Uuid;

use crate::{ApiState, Principal};

/// The route, as stored in the idempotency ledger.
const ROUTE: &str = "/v2/captures";

/// What the submitted work is. Present tense: a kind names an activity, not a completed fact.
const OPERATION_KIND: &str = "content.capture.submit";

/// The command this route emits. The extractor is its consumer.
const COMMAND_TYPE: &str = "content.capture.requested.v1";

/// The header `INTERFACES.md` requires on a replayable mutation.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// What a client submits.
#[derive(Debug, serde::Deserialize)]
pub struct SubmitCapture {
    /// The address to capture.
    pub url: String,
}

/// What a client gets back.
///
/// `ARCHITECTURE.md` S5.1: the API acknowledges durable ACCEPTANCE, not completion. The body
/// therefore carries an operation to poll and nothing that could be mistaken for a result.
#[derive(Debug, serde::Serialize)]
pub struct CaptureAccepted {
    /// The operation to poll at `/v2/operations/{id}`.
    pub operation_id: Uuid,
    /// Always `accepted` here. Present so a client never has to infer it from the status code.
    pub status: &'static str,
}

/// `POST /v2/captures`.
///
/// Refuses rather than guesses: no `Idempotency-Key` is a client error, because a replayable
/// mutation without one is an unprotected write that only looks safe.
pub async fn submit(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    headers: HeaderMap,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let (key, submit) = match parse(&headers, &body) {
        Ok(parsed) => parsed,
        Err(kind) => return platform_http::reject(kind),
    };
    let key = key.as_str();

    let now = jiff::Timestamp::now();
    let Ok(subject) = Subject::new(MessageClass::Command, COMMAND_TYPE) else {
        tracing::error!(
            command = COMMAND_TYPE,
            "the command subject is not constructible"
        );
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let reservation = match platform_idempotency::reserve(
        &mut transaction,
        principal.user_id,
        ROUTE,
        OPERATION_KIND,
        Digest::of_key(key),
        Digest::of_body(&body),
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

    let record_id = match settle(&reservation) {
        Settled::Proceed(record_id) => record_id,
        Settled::Answer(response) => return response,
    };

    // The correlation the middleware already minted for this request (ADR-0007). `Option` because
    // a unit test may call the handler without the middleware; in production it is always present.
    let correlation = context.map_or_else(
        || platform_telemetry::correlation::mint_correlation().to_string(),
        |axum::Extension(context)| context.correlation_id.to_string(),
    );

    let operation = match platform_operations::accept(
        &mut *transaction,
        principal.user_id,
        OPERATION_KIND,
        &correlation,
        Some(key),
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

    let payload = command(
        &operation,
        principal.user_id,
        &correlation,
        key,
        &submit.url,
        now,
    );

    if let Err(error) = Outbox::enqueue(
        &mut *transaction,
        Uuid::now_v7(),
        &subject,
        &payload,
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

    if let Err(error) = transaction.commit().await {
        // Nothing happened: no reservation, no operation, no command. The client may retry with the
        // same key and get a clean first attempt.
        tracing::error!(%error, "the capture transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    accepted(operation.operation_id)
}

/// Turn a reservation into either a record to complete, or the answer to send instead.
///
/// Split out of [`submit`] so that function stays inside the workspace's length lint, and along the
/// boundary that means something: everything before it decides WHETHER to do the work, everything
/// after it does the work.
fn settle(reservation: &Reservation) -> Settled {
    match *reservation {
        Reservation::Fresh { record_id } => Settled::Proceed(record_id),
        // The original answer, not a new operation. S8.1: "Retrying the same payload returns the
        // original operation."
        Reservation::Replay {
            operation_id: Some(operation_id),
            ..
        } => Settled::Answer(accepted(operation_id)),
        // A completed reservation with no operation means the first attempt was refused before it
        // created one. Replaying its refusal is more truthful than starting work the first attempt
        // declined to start.
        Reservation::Replay { .. } | Reservation::InFlight | Reservation::Conflict => {
            Settled::Answer(platform_http::reject(FailureKind::IdempotencyConflict))
        }
    }
}

/// The command document.
///
/// A typed command envelope arrives when `ratatoskr-contracts` ships one; until then this carries
/// exactly the members `ARCHITECTURE.md` S5.3 names, and the subject already fixes its type and its
/// version.
fn command(
    operation: &platform_operations::Operation,
    tenant: Uuid,
    correlation: &str,
    idempotency_key: &str,
    url: &str,
    now: jiff::Timestamp,
) -> serde_json::Value {
    serde_json::json!({
        "command_id": Uuid::now_v7(),
        "command_type": COMMAND_TYPE,
        "requested_at": now.to_string(),
        "operation_id": operation.operation_id,
        "tenant_id": format!("user:{tenant}"),
        "correlation_id": correlation,
        "idempotency_key": idempotency_key,
        "payload": { "url": url },
    })
}

/// What a reservation means for the rest of the handler.
///
/// An enum rather than a `Result`, because a replay is not an error: it is a successful answer that
/// happens to skip the work.
enum Settled {
    /// Do the work, then complete this ledger row.
    Proceed(Uuid),
    /// Send this instead.
    Answer(Response),
}

/// Read the idempotency key and the body, or say which client error this is.
///
/// The body is parsed from raw bytes rather than through `Json<T>`, because the fingerprint must be
/// taken over exactly what the client sent: re-serializing a parsed value would make two
/// byte-different requests look identical to the ledger.
fn parse(
    headers: &HeaderMap,
    body: &axum::body::Bytes,
) -> Result<(String, SubmitCapture), FailureKind> {
    let key = headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 255)
        .ok_or(FailureKind::MissingIdempotencyKey)?
        .to_owned();

    let submit: SubmitCapture =
        serde_json::from_slice(body).map_err(|_| FailureKind::InvalidRequest)?;
    if !is_capturable(&submit.url) {
        return Err(FailureKind::InvalidRequest);
    }
    Ok((key, submit))
}

/// The 202 body. `ARCHITECTURE.md` S5.3: `202 Accepted` for asynchronous work.
fn accepted(operation_id: Uuid) -> Response {
    (
        http::StatusCode::ACCEPTED,
        Json(CaptureAccepted {
            operation_id,
            status: "accepted",
        }),
    )
        .into_response()
}

/// Whether Platform will hand this address to the extractor.
///
/// Deliberately shallow. Platform routes; it does not fetch, and `ARCHITECTURE.md` S15 says Edge
/// "does not render or inspect active content". The real defence — SSRF policy, redirect handling,
/// content limits — belongs to `ratatoskr-extractor`, which is the process that will actually open
/// the connection. Rejecting an obviously unusable scheme here just avoids creating an operation
/// that can only fail.
fn is_capturable(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.host().is_some() && raw.len() <= 2048
}
