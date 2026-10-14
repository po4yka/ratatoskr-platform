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
use platform_api_doc::{Method, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_identity::audit::{self, AuditEvent, AuditOutcome};
use ratatoskr_error_contracts::{ErrorCode, ErrorEnvelope};
use tracing::Instrument as _;
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

/// The path of the connect call.
pub const CONNECT_ROUTE: &str = "/v1/notes/connections";

/// The body of a connect. The notes service owns its members, and Edge sends it to the notes service
/// unread.
#[derive(Debug, schemars::JsonSchema)]
#[schemars(rename = "NotesConnectRequest")]
pub struct ConnectRequest(pub serde_json::Map<String, serde_json::Value>);

/// What a client gets back from a connect that the notes service accepted.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "NotesConnectAccepted")]
pub struct ConnectAccepted {
    /// The operation to poll at `/v1/operations/{id}`.
    pub operation_id: Uuid,
    /// Always `accepted` here.
    pub status: &'static str,
    /// The answer of the notes service, unchanged: the connection id, its channel and its state.
    pub connection: serde_json::Value,
}

/// How the connect call is described in the generated `OpenAPI` document.
///
/// The route is not in the route table of [`crate::routes`]: Edge serves it only when the
/// deployment routes the notes service, and it takes the `control` body budget of that route.
pub const CONNECT_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: CONNECT_ROUTE,
    operation_id: "connectNotesVault",
    summary: "Connect an Obsidian vault",
    description: "\
Starts the connection of one Obsidian vault to the notes service. Edge records an operation of kind \
`notes.connection.connect`, sends the body to the notes service unchanged with the operation id, and \
keeps no part of the body: the body holds the Obsidian password, the MFA code and the end-to-end \
password, so Edge does not store, audit or log it. The notes service owns the members of the body \
and reports the phases of the connection on the operation; poll `GET /v1/operations/{operation_id}`.\n\n\
There is no `Idempotency-Key`: the ledger would keep a digest of the body. The notes service refuses \
a second connect of one vault. When the notes service does not take the connect, Edge terminates the \
operation as `failed` with the code of the failure before it answers.\n\n\
Edge serves this route only when the deployment routes the notes service. The body budget is the \
`control` budget of that route.",
    tag: "notes",
    security: Security::Session,
    parameters: &[],
    request: Some(Payload::Json("NotesConnectRequest")),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "The notes service accepted the connect. The body carries the operation \
                          to poll and the answer of the notes service.",
            payload: Some(Payload::Json("NotesConnectAccepted")),
        },
        ResponseDoc {
            status: 400,
            description: "The notes service refused the body, and the `code` in the envelope is \
                          the code of the notes service. Or the body failed while Edge sent it, \
                          for example because the client stopped the upload \
                          (`platform.request.invalid`). The operation is `failed`.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The notes service refused the connect, for example \
                          `notes.connection.vault_already_connected`.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 413,
            description: "The body is larger than the `control` budget of the notes route \
                          (`platform.request.payload_too_large`).",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "This caller has spent its request allowance.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 502,
            description: "The notes service gave an answer that Edge cannot use \
                          (`edge.upstream_invalid_response`): a status other than `202` that is \
                          not an error envelope, or a `202` that is not JSON. Edge does not relay \
                          a redirect. The operation is `failed`.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 503,
            description: "The notes service could not be reached \
                          (`edge.upstream_unavailable`). The operation is `failed`.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "The notes service did not answer within the `control` budget \
                          (`edge.upstream_timeout`). The operation is `failed`.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// `POST /v1/notes/connections`.
///
/// When the forward fails after the operation is minted, the operation is terminated as `failed`
/// before Edge answers, so a client that polls it does not wait for the stale-operation reaper. The
/// error envelope has no field for the operation id (contracts ADR-0008 keeps its `extensions`
/// empty), so a failed connect does not name it.
///
/// The forward and the terminal write run in a task of their own. A client that disconnects drops
/// this handler, and the task still ends the operation.
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
    let forward = async move {
        let claims = Claims {
            principal,
            device_kind,
            correlation_id: &correlation_id,
        };
        forward(&state, request, claims, operation_id).await
    };
    match tokio::spawn(forward.in_current_span()).await {
        Ok(response) => response,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            // Only a runtime that shuts down cancels the task. The reaper ends the operation.
            Err(_) => platform_http::reject(FailureKind::UpstreamUnavailable),
        },
    }
}

/// Send the connect to notes and answer what it answered.
///
/// A notes error envelope is relayed as the generic proxy relays it. Any other answer than `202`
/// and an error is an invalid upstream response: a redirect is not relayed, because the client
/// would send its secrets to the place it names. A `202` answer that is not JSON is an invalid
/// upstream response too.
async fn forward(
    state: &ApiState,
    request: Request,
    claims: Claims<'_>,
    operation_id: Uuid,
) -> Response {
    let response = match state
        .gateway
        .send_notes_connect(request, claims, operation_id)
        .await
    {
        Ok(response) => response,
        Err(kind) => return refuse(state, operation_id, kind).await,
    };
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        let relayed = gateway::response_from_upstream(response, ROUTE).await;
        return relay_refusal(state, operation_id, relayed).await;
    }
    if status != StatusCode::ACCEPTED {
        tracing::warn!(
            route = ROUTE,
            class = "unexpected_status",
            "a connect answer had a status other than 202"
        );
        return refuse(state, operation_id, FailureKind::UpstreamInvalidResponse).await;
    }
    let Ok(answer) = axum::body::to_bytes(Body::new(response.into_body()), MAX_ANSWER_BYTES).await
    else {
        tracing::warn!(
            route = ROUTE,
            class = "oversized_body",
            "a connect answer exceeded its bound"
        );
        return refuse(state, operation_id, FailureKind::UpstreamInvalidResponse).await;
    };
    let Ok(connection) = serde_json::from_slice::<serde_json::Value>(&answer) else {
        tracing::warn!(
            route = ROUTE,
            class = "invalid_json",
            "a connect answer was not JSON"
        );
        return refuse(state, operation_id, FailureKind::UpstreamInvalidResponse).await;
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
    let bytes = axum::body::to_bytes(body, gateway::MAX_ERROR_ENVELOPE_BYTES)
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
