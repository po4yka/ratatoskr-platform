//! `POST /v1/notes/connections`: Edge in front of the connect call of `ratatoskr-notes`.
//!
//! Every other `/v1/notes` request goes through the generic proxy. The connect call is an Edge
//! handler because its body holds the Obsidian password, the MFA code and the end-to-end password
//! (workspace change `add-notes-vault-connections`, design D4 and D10). Edge mints the operation,
//! streams the body to the notes listener unchanged, and keeps no part of it: the body is not read,
//! not buffered and not logged. The operation has no idempotency key, because the ledger would keep
//! a digest of the body, and its audit record names only the operation.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::response::{IntoResponse as _, Response};
use http::StatusCode;
use platform_core::FailureKind;
use platform_identity::audit::{self, AuditEvent, AuditOutcome};
use ratatoskr_error_contracts::{ErrorCode, ErrorEnvelope};
use uuid::Uuid;

use crate::gateway::{self, Claims};
use crate::{ApiState, Principal};

/// The fixed label of the notes route in a log line.
const ROUTE: &str = "/v1/notes";

/// The kind of the operation a connect mints.
const OPERATION_KIND: &str = "notes.connection.connect";

/// The one service that reports on a connect operation (XR-021 R3-02).
const NOTES_PRODUCER: &str = "ratatoskr-notes";

/// The largest success answer of the notes service that Edge reads. A connect answer holds an id,
/// a channel and a state.
const MAX_ANSWER_BYTES: usize = 16_384;

/// The largest error answer that Edge reads back to find its code. The generic proxy relays an
/// error envelope only up to this size.
const MAX_ERROR_BYTES: usize = 65_536;

/// What a client gets back from a connect that the notes service accepted.
#[derive(Debug, serde::Serialize)]
struct ConnectAccepted {
    /// The operation to poll at `/v1/operations/{id}`.
    operation_id: Uuid,
    /// Always `accepted` here.
    status: &'static str,
    /// The answer of the notes service, unchanged.
    connection: serde_json::Value,
}

/// `POST /v1/notes/connections`.
///
/// A notes error envelope is relayed as the generic proxy relays it. A success answer that is not
/// JSON is an invalid upstream response. When the forward fails after the operation is minted, the
/// operation is terminated as `failed` before Edge answers, so a client that polls it does not wait
/// for the stale-operation reaper. The error envelope has no field for the operation id
/// (contracts ADR-0008 keeps its `extensions` empty), so a failed connect does not name it.
pub async fn connect(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Extension(context): Extension<platform_http::RequestContext>,
    request: Request,
) -> Response {
    let device_kind = match gateway::device_kind(&state, principal).await {
        Ok(kind) => kind,
        Err(rejection) => return rejection,
    };
    let correlation_id = context.correlation_id.to_string();
    let operation_id = match mint(&state, principal, &correlation_id).await {
        Ok(operation_id) => operation_id,
        Err(rejection) => return rejection,
    };
    let claims = Claims {
        principal,
        device_kind,
        correlation_id: &correlation_id,
    };
    let response = match state
        .gateway
        .send_notes_connect(request, claims, operation_id)
        .await
    {
        Ok(response) => response,
        Err(kind) => return refuse(&state, operation_id, kind).await,
    };
    if !response.status().is_success() {
        let relayed = gateway::response_from_upstream(response, ROUTE).await;
        return relay_refusal(&state, operation_id, relayed).await;
    }
    let Ok(answer) = axum::body::to_bytes(Body::new(response.into_body()), MAX_ANSWER_BYTES).await
    else {
        tracing::warn!(
            route = ROUTE,
            class = "oversized_body",
            "a connect answer exceeded its bound"
        );
        return refuse(&state, operation_id, FailureKind::UpstreamInvalidResponse).await;
    };
    let Ok(connection) = serde_json::from_slice::<serde_json::Value>(&answer) else {
        tracing::warn!(
            route = ROUTE,
            class = "invalid_json",
            "a connect answer was not JSON"
        );
        return refuse(&state, operation_id, FailureKind::UpstreamInvalidResponse).await;
    };
    (
        StatusCode::ACCEPTED,
        Json(ConnectAccepted {
            operation_id,
            status: "accepted",
            connection,
        }),
    )
        .into_response()
}

/// Terminate the operation with the code of an Edge fault, and answer that fault.
async fn refuse(state: &ApiState, operation_id: Uuid, kind: FailureKind) -> Response {
    let fault = kind.fault();
    fail(state, operation_id, &fault.code, fault.retryable).await;
    platform_http::reject(kind)
}

/// Terminate the operation with the code of the error that notes answered, and relay that error.
///
/// `relayed` is what the generic proxy makes of the answer: the notes error envelope when it is
/// one, or an Edge fault with an empty body. The body is in memory already or bounded here.
async fn relay_refusal(state: &ApiState, operation_id: Uuid, relayed: Response) -> Response {
    let (parts, body) = relayed.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_ERROR_BYTES)
        .await
        .unwrap_or_default();
    let (code, retryable) = serde_json::from_slice::<ErrorEnvelope>(&bytes).map_or_else(
        |_| {
            let fault = FailureKind::UpstreamInvalidResponse.fault();
            (fault.code.clone(), fault.retryable)
        },
        |envelope| (envelope.code, envelope.retryable),
    );
    fail(state, operation_id, &code, retryable).await;
    Response::from_parts(parts, Body::from(bytes))
}

/// Terminate the operation as `failed`. When that fails, the stale-operation reaper terminates it
/// later, so the client still gets the answer of the connect.
async fn fail(state: &ApiState, operation_id: Uuid, code: &ErrorCode, retryable: bool) {
    if let Err(error) = platform_operations::fail_notes_connect(
        state.database.pool(),
        operation_id,
        code,
        retryable,
        jiff::Timestamp::now(),
    )
    .await
    {
        tracing::error!(%error, "a failed connect operation could not be terminated");
    }
}

/// Record the operation and its audit event in one transaction, before the notes service can report
/// on the operation.
async fn mint(
    state: &ApiState,
    principal: Principal,
    correlation_id: &str,
) -> Result<Uuid, Response> {
    let refused = || platform_http::reject(FailureKind::RequestTimeout);
    let now = jiff::Timestamp::now();
    let mut transaction = state.database.pool().begin().await.map_err(|error| {
        tracing::error!(%error, "a connect transaction could not be started");
        refused()
    })?;
    let operation = platform_operations::accept(
        &mut *transaction,
        principal.user_id,
        OPERATION_KIND,
        correlation_id,
        None,
        Some(NOTES_PRODUCER),
        now,
    )
    .await
    .map_err(|error| {
        tracing::error!(%error, "a connect operation could not be accepted");
        refused()
    })?;
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: OPERATION_KIND,
        target_kind: "operation",
        target_id: Some(operation.operation_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation_id.to_owned(),
    };
    audit::record(&mut *transaction, &event, now)
        .await
        .map_err(|error| {
            tracing::error!(%error, "a connect could not be audited");
            refused()
        })?;
    transaction.commit().await.map_err(|error| {
        tracing::error!(%error, "a connect transaction could not be committed");
        refused()
    })?;
    Ok(operation.operation_id)
}
