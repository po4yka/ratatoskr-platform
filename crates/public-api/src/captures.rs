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
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_eventing::{MessageClass, Outbox, Subject};
use platform_idempotency::{Digest, Outcome};
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
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SubmitCapture {
    /// The address to capture. `http` or `https`, with a host, at most 2048 characters.
    pub url: String,
}

/// What a client gets back.
///
/// `ARCHITECTURE.md` S5.1: the API acknowledges durable ACCEPTANCE, not completion. The body
/// therefore carries an operation to poll and nothing that could be mistaken for a result.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
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

    let correlation = crate::correlation_of(context);

    accept(&state, principal, &key, &submit, &body, &correlation).await
}

/// Reserve, create, enqueue, complete — in one transaction, so a crash at any point leaves all four
/// or none.
///
/// Split from [`submit`] along the boundary that means something: everything before it decides
/// whether this request is one we accept, and this does the work.
async fn accept(
    state: &ApiState,
    principal: Principal,
    key: &str,
    submit: &SubmitCapture,
    body: &[u8],
    correlation: &str,
) -> Response {
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
        Digest::of_body(body),
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
        Outcome::Replay(operation_id) => return accepted(operation_id),
        Outcome::Refuse => return platform_http::reject(FailureKind::IdempotencyConflict),
    };

    let operation = match platform_operations::accept(
        &mut *transaction,
        principal.user_id,
        OPERATION_KIND,
        correlation,
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

    let payload = platform_eventing::Command {
        command_type: COMMAND_TYPE,
        operation_id: operation.operation_id,
        principal: principal.user_id,
        correlation_id: correlation,
        idempotency_key: key,
        requested_at: now,
    }
    .envelope(serde_json::json!({ "url": submit.url }));

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
    if !platform_core::address::is_capturable(&submit.url) {
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

/// How this route is described in the generated `OpenAPI` document.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: ROUTE,
    operation_id: "submitCapture",
    summary: "Submit an address for capture",
    description: "\
Accepts the address durably and returns the operation that tracks it. It does NOT return a result: \
the work happens elsewhere, and `GET /v2/operations/{operation_id}` is where its outcome appears.\n\n\
`Idempotency-Key` is required, not optional. A capture is a replayable mutation, and a retry \
without a key is a second operation that looks like the first. Retrying with the same key and the \
same body returns the ORIGINAL operation; the same key with a different body is refused, because \
honouring it would silently replace the meaning of a request already sent.\n\n\
The address is checked only for a usable scheme and host. Fetching it, following its redirects and \
bounding what it returns belong to the service that opens the connection, not to this one.",
    tag: "captures",
    security: Security::Session,
    parameters: &[Parameter {
        name: "Idempotency-Key",
        location: In::Header,
        required: true,
        format: None,
        description: "A client-chosen key, 1 to 255 characters, unique per distinct request. It is \
                      hashed before it is stored, so it may carry meaning the client considers \
                      private.",
    }],
    request: Some(Payload::Json("SubmitCapture")),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "Accepted durably. The body carries the operation to poll.",
            payload: Some(Payload::Json("CaptureAccepted")),
        },
        ResponseDoc {
            status: 400,
            description: "No `Idempotency-Key`, a body that is not readable, or an address this \
                          API will not accept. The `code` in the envelope distinguishes them.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The key is in use for a different body, or an earlier attempt with it \
                          has not finished.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written; retrying with \
                          the same key is safe and is the intended response.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
