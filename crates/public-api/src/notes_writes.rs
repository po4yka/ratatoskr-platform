//! The write routes of `ratatoskr-notes` at Edge (workspace change `add-notes-write-back`).
//!
//! A write and a conflict resolution are Platform operations of kind `notes.file.write`. Edge
//! checks the body against `notes/write-request.v1`, and then, in one transaction, reserves the
//! idempotency key, mints the operation and enqueues `notes.file.write_requested.v1` with the
//! session's user as tenant and `origin` `platform`. The command names its content by a `BlobRef`
//! that the client uploaded to notes first, so no byte of a note enters the outbox or the bus.
//! Edge holds no notes data: notes checks that the tenant owns the connection and the conflict.
//!
//! The write-mode switch is a route of notes. Edge mints the operation
//! `notes.connection.write_mode` in the same kind of transaction, without a command, and forwards
//! the request with the operation id; notes reports the operation.
//!
//! A device credential is refused on all three routes. The plugin and the export agent are the
//! channels of a vault and call only the device routes (`crate::notes::refuses`).

use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse as _, Response};
use http::HeaderMap;
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_eventing::{MessageClass, PLATFORM_PRODUCER, Subject};
use ratatoskr_event_envelope::{
    CommandEnvelope, CommandPayload, EnvelopeSchemaVersion, ProducerName,
};
use ratatoskr_identifiers::{
    BlobRef, CommandId, ContentDigest, DigestAlgorithm, DigestHex, Extensions, NotesConflictId,
    NotesConnectionId, OperationId, TenantRef, UserId, WireTimestamp,
};
use ratatoskr_notes_contracts::{
    FileWriteRequested, ResolutionChoice, ResolveConflict, WriteOperation, WriteOrigin,
};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::intake::{Intake, Minted, Prepared, accept, mint};
use crate::notes::{Claimed, Forward};
use crate::{ApiState, Principal};

/// The router path of a write. The parameter has the name of the notes transfer paths, because
/// one segment of the router takes one parameter name.
pub(crate) const WRITES_PATH: &str = "/v1/notes/connections/{connection}/writes";

/// The router path of a conflict resolution.
pub(crate) const RESOLUTION_PATH: &str = "/v1/notes/conflicts/{conflict}/resolution";

/// The router path of the write-mode switch.
pub(crate) const WRITE_MODE_PATH: &str = "/v1/notes/connections/{connection}/write-mode";

/// The route strings that the idempotency ledger stores: plain segments, because the ledger refuses
/// a template. The connection or the conflict in the path is part of the fingerprint instead.
///
/// A write and a resolution share one ledger route. Both are one `notes.file.write`, notes
/// deduplicates a write by its key and connection, and the operations table holds one key per owner
/// and kind. So a key that one route took is refused on the other as a conflict, and never becomes a
/// second operation.
const FILE_WRITE_LEDGER_ROUTE: &str = "/v1/notes/writes";
const WRITE_MODE_LEDGER_ROUTE: &str = "/v1/notes/connections/write-mode";

/// The kind of the operation that a write or a resolution mints.
const WRITE_KIND: &str = "notes.file.write";

/// The kind of the operation that a write-mode change mints.
const WRITE_MODE_KIND: &str = "notes.connection.write_mode";

/// The one service that reports on the operations of these routes (XR-021 R3-02).
const NOTES_PRODUCER: &str = "ratatoskr-notes";

/// The header `INTERFACES.md` requires on a replayable mutation.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// What a client submits to change one file of a connected vault.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "NotesWriteRequest")]
pub struct WriteRequest {
    /// The one change: an object with one member, whose name is the operation kind (`create`,
    /// `update`, `rename`, `move`, `delete`, `set_property` or `resolve_conflict`).
    pub operation: WriteOperation,
}

/// What a client submits to end an open conflict.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "NotesResolutionRequest")]
pub struct ResolutionRequest {
    /// The connection whose vault holds the file of the conflict, as
    /// `GET /v1/notes/conflicts/{conflict_id}` names it. Edge holds no notes data; notes refuses a
    /// connection that does not hold the conflict.
    pub connection_id: NotesConnectionId,
    /// How the owner resolves the conflict.
    pub choice: ResolutionChoice,
    /// The content to write, uploaded first to notes. Present exactly when `choice` is `custom`.
    #[serde(default)]
    pub blob: Option<BlobRef>,
}

/// Whether Ratatoskr may write the vault of a connection.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Writes are delivered.
    On,
    /// Writes are refused, and every write not yet delivered fails.
    Off,
}

/// What a client submits to turn write mode on or off. Notes owns the member; Edge checks it so
/// that it mints no operation for a body that notes cannot take.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "NotesWriteModeRequest")]
pub struct WriteModeRequest {
    /// The mode to set.
    pub write_mode: WriteMode,
}

/// What a client gets back from the three routes: an operation to poll, never a result.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "NotesWriteAccepted")]
pub struct WriteAccepted {
    /// The operation to poll at `/v1/operations/{id}`.
    pub operation_id: Uuid,
    /// Always `accepted` here.
    pub status: &'static str,
}

/// The `202` body of the three routes.
pub(crate) fn accepted(operation_id: Uuid) -> Response {
    (
        http::StatusCode::ACCEPTED,
        Json(WriteAccepted {
            operation_id,
            status: "accepted",
        }),
    )
        .into_response()
}

/// `POST /v1/notes/connections/{connection_id}/writes`.
pub async fn write(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(connection): Path<String>,
    headers: HeaderMap,
    context: Option<Extension<platform_http::RequestContext>>,
    body: Bytes,
) -> Response {
    let prepared = admissible(principal, &headers).and_then(|key| {
        let connection_id =
            NotesConnectionId::parse(&connection).map_err(|_| FailureKind::InvalidRequest)?;
        let request = serde_json::from_slice::<WriteRequest>(&body)
            .map_err(|_| FailureKind::InvalidRequest)?;
        let command = command(connection_id, request.operation, &key)?;
        Ok((key, command))
    });
    let (key, command) = match prepared {
        Ok(prepared) => prepared,
        Err(kind) => return platform_http::reject(kind),
    };
    let fingerprint = fingerprint(&format!("writes/{connection}"), &body);
    let submission = Submission {
        key: &key,
        fingerprint: &fingerprint,
    };
    submit(&state, principal, context, &submission, command).await
}

/// `POST /v1/notes/conflicts/{conflict_id}/resolution`.
pub async fn resolve(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(conflict): Path<String>,
    headers: HeaderMap,
    context: Option<Extension<platform_http::RequestContext>>,
    body: Bytes,
) -> Response {
    let prepared = admissible(principal, &headers).and_then(|key| {
        let conflict_id =
            NotesConflictId::parse(&conflict).map_err(|_| FailureKind::InvalidRequest)?;
        let request = serde_json::from_slice::<ResolutionRequest>(&body)
            .map_err(|_| FailureKind::InvalidRequest)?;
        let operation = WriteOperation::ResolveConflict(ResolveConflict {
            conflict_id,
            choice: request.choice,
            blob: request.blob,
        });
        let command = command(request.connection_id, operation, &key)?;
        Ok((key, command))
    });
    let (key, command) = match prepared {
        Ok(prepared) => prepared,
        Err(kind) => return platform_http::reject(kind),
    };
    let fingerprint = fingerprint(&format!("resolution/{conflict}"), &body);
    let submission = Submission {
        key: &key,
        fingerprint: &fingerprint,
    };
    submit(&state, principal, context, &submission, command).await
}

/// `PUT /v1/notes/connections/{connection_id}/write-mode`.
///
/// A replay answers the original operation and does not forward again. A first request is
/// forwarded as a connect is ([`crate::notes::settle`]): in a task of its own, so the operation
/// ends even when the client leaves, and a failed forward terminates the operation before Edge
/// answers.
pub async fn set_write_mode(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(connection): Path<String>,
    uri: http::Uri,
    headers: HeaderMap,
    Extension(context): Extension<platform_http::RequestContext>,
    body: Bytes,
) -> Response {
    let key = match admissible(principal, &headers).and_then(|key| {
        NotesConnectionId::parse(&connection).map_err(|_| FailureKind::InvalidRequest)?;
        serde_json::from_slice::<WriteModeRequest>(&body)
            .map_err(|_| FailureKind::InvalidRequest)?;
        Ok(key)
    }) {
        Ok(key) => key,
        Err(kind) => return platform_http::reject(kind),
    };
    let correlation_id = context.correlation_id.to_string();
    let fingerprint = fingerprint(&connection, &body);
    let intake = Intake {
        route: WRITE_MODE_LEDGER_ROUTE,
        kind: WRITE_MODE_KIND,
        audit_action: WRITE_MODE_KIND,
        key: &key,
        fingerprint: &fingerprint,
        correlation: &correlation_id,
        reporter: Some(NOTES_PRODUCER),
    };
    let operation_id = match mint(&state, principal, &intake).await {
        Ok(Minted::Fresh(operation_id)) => operation_id,
        Ok(Minted::Replayed(operation_id)) => return accepted(operation_id),
        Err(kind) => return platform_http::reject(kind),
    };
    let mut request = http::Request::new(Body::from(body));
    *request.method_mut() = http::Method::PUT;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    let claimed = Claimed {
        principal,
        device_kind: None,
        correlation_id,
    };
    crate::notes::settle(state, request, claimed, operation_id, Forward::WriteMode).await
}

/// The caller's idempotency key, or the refusal. A device is refused first, before anything about
/// its request is read.
fn admissible(principal: Principal, headers: &HeaderMap) -> Result<String, FailureKind> {
    if principal.device_id.is_some() {
        return Err(FailureKind::Forbidden);
    }
    headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 255)
        .map(str::to_owned)
        .ok_or(FailureKind::MissingIdempotencyKey)
}

/// What the request acts on, and the body: the same key and body for another connection, another
/// conflict or the other write route is a conflict, not a replay.
fn fingerprint(selector: &str, body: &[u8]) -> Vec<u8> {
    let mut fingerprint = selector.as_bytes().to_vec();
    fingerprint.push(b'\n');
    fingerprint.extend_from_slice(body);
    fingerprint
}

/// The command payload of a write, checked against every rule of `notes/write-request.v1` before
/// any operation exists. The operation id is set once the operation is minted.
fn command(
    connection_id: NotesConnectionId,
    operation: WriteOperation,
    key: &str,
) -> Result<FileWriteRequested, FailureKind> {
    let command = FileWriteRequested {
        operation_id: OperationId(Uuid::nil()),
        idempotency_key: key_digest(key)?,
        connection_id,
        origin: WriteOrigin::parse("platform").map_err(|_| FailureKind::RequestTimeout)?,
        operation,
    };
    command
        .validate()
        .map_err(|_| FailureKind::InvalidRequest)?;
    Ok(command)
}

/// How a write is recorded in the idempotency ledger.
struct Submission<'a> {
    key: &'a str,
    fingerprint: &'a [u8],
}

/// Reserve the key, mint the operation and enqueue the command, in one transaction.
async fn submit(
    state: &ApiState,
    principal: Principal,
    context: Option<Extension<platform_http::RequestContext>>,
    submission: &Submission<'_>,
    mut command: FileWriteRequested,
) -> Response {
    let correlation = crate::correlation_of(context);
    let intake = Intake {
        route: FILE_WRITE_LEDGER_ROUTE,
        kind: WRITE_KIND,
        audit_action: WRITE_KIND,
        key: submission.key,
        fingerprint: submission.fingerprint,
        correlation: &correlation,
        reporter: Some(NOTES_PRODUCER),
    };
    accept(state, principal, &intake, accepted, |operation, now| {
        command.operation_id = OperationId(operation.operation_id);
        envelope(&command, principal.user_id, now)
    })
    .await
}

/// The SHA-256 of the caller's `Idempotency-Key`, as the command carries it.
fn key_digest(key: &str) -> Result<ContentDigest, FailureKind> {
    let hex = DigestHex::parse(&format!("{:x}", Sha256::digest(key.as_bytes())))
        .map_err(|_| FailureKind::RequestTimeout)?;
    Ok(ContentDigest {
        algorithm: DigestAlgorithm::Sha256,
        hex,
    })
}

/// The `CommandEnvelope` of a write: aggregate and correlation `operation:<id>`, tenant the user.
fn envelope(
    command: &FileWriteRequested,
    user: Uuid,
    now: jiff::Timestamp,
) -> Result<Prepared, FailureKind> {
    fn internal<E>(_: E) -> FailureKind {
        tracing::error!("a notes write command could not be built");
        FailureKind::RequestTimeout
    }
    let serde_json::Value::Object(payload) = serde_json::to_value(command).map_err(internal)?
    else {
        return Err(FailureKind::RequestTimeout);
    };
    let command_id = Uuid::now_v7();
    let operation = command.operation_id.as_entity_ref();
    let envelope = CommandEnvelope {
        command_id: CommandId(command_id),
        command_type: FileWriteRequested::command_type(),
        issued_at: WireTimestamp::from_jiff(now),
        producer: ProducerName::parse(PLATFORM_PRODUCER).map_err(internal)?,
        aggregate_id: operation.clone(),
        correlation_id: operation,
        causation_id: None,
        tenant_id: Some(TenantRef::of_user(UserId(user))),
        schema_version: EnvelopeSchemaVersion::CURRENT,
        payload,
        extensions: Extensions::new(),
    };
    Ok(Prepared {
        subject: Subject::new(MessageClass::Command, FileWriteRequested::COMMAND_TYPE)
            .map_err(internal)?,
        command_id,
        payload: serde_json::to_value(envelope).map_err(internal)?,
    })
}

/// The `Idempotency-Key` of the three routes.
const IDEMPOTENCY_PARAMETER: Parameter = Parameter {
    name: "Idempotency-Key",
    location: In::Header,
    required: true,
    format: None,
    description: "A client-chosen key, 1 to 255 characters, unique per distinct request. It is \
                  hashed before it is stored, so it may carry meaning the client considers private.",
};

/// The connection in the path of a write and of the write-mode switch.
const CONNECTION_PARAMETER: Parameter = Parameter {
    name: "connection_id",
    location: In::Path,
    required: true,
    format: Some("uuid"),
    description: "The vault connection.",
};

/// One documented answer.
const fn response(status: u16, description: &'static str, payload: &'static str) -> ResponseDoc {
    ResponseDoc {
        status,
        description,
        payload: Some(Payload::Json(payload)),
    }
}

/// The answers that a write and a resolution share.
const WRITE_RESPONSES: &[ResponseDoc] = &[
    response(
        202,
        "Accepted durably. The body carries the operation to poll.",
        "NotesWriteAccepted",
    ),
    response(
        400,
        "No `Idempotency-Key`, an id in the path that is not a UUID, or a body that fails \
         `notes/write-request.v1`. The `code` in the envelope distinguishes them.",
        "ErrorEnvelope",
    ),
    response(
        401,
        "No credential, or one that does not authenticate here.",
        "ErrorEnvelope",
    ),
    response(
        403,
        "A device credential. A device calls only the device routes of notes.",
        "ErrorEnvelope",
    ),
    response(
        409,
        "The key is in use for a different request, or an earlier attempt with it has not \
         finished.",
        "ErrorEnvelope",
    ),
    response(
        413,
        "The body is larger than the `control` budget of the notes route.",
        "ErrorEnvelope",
    ),
    response(
        429,
        "This caller has spent its request allowance.",
        "ErrorEnvelope",
    ),
    response(
        504,
        "A dependency did not answer in time. Nothing was written; retrying with the same key is \
         safe.",
        "ErrorEnvelope",
    ),
];

/// How the write route is described in the generated `OpenAPI` document.
///
/// Like the connect, the three notes routes are not in the route table of [`crate::routes`]: Edge
/// serves them only when the deployment routes the notes service, with the `control` body budget
/// of that route.
pub const WRITES_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/notes/connections/{connection_id}/writes",
    operation_id: "writeNotesFile",
    summary: "Change one file of a connected vault",
    description: "\
Accepts one change of one file durably and returns the operation of kind `notes.file.write` that \
tracks it; the notes service reports the outcome on the operation. The body names exactly one \
operation of the contract `notes/write-request.v1`: `create`, `update`, `rename`, `move`, `delete`, \
`set_property` or `resolve_conflict`. Every operation on an existing file names the `base_hash` that \
the writer read, and a write on an older revision ends as a conflict, never as an overwrite.\n\n\
Content never travels in this request. Upload the bytes first to \
`/v1/notes/connections/{connection_id}/blobs`, and name the resulting `BlobRef` (owner \
`ratatoskr-notes`); a frontmatter value is uploaded as one `application/json` document. Edge sets \
`origin` to `platform`. Edge holds no notes data: the notes service fails the operation with \
`notes.connection.not_found` for a connection that the caller does not own, and with \
`notes.write.disabled` while the write mode of the connection is off.\n\n\
`Idempotency-Key` is required. The same key and body return the ORIGINAL operation; the same key \
with another body, another connection, or on `POST /v1/notes/conflicts/{conflict_id}/resolution` \
is refused with `409`.",
    tag: "notes",
    security: Security::Session,
    parameters: &[CONNECTION_PARAMETER, IDEMPOTENCY_PARAMETER],
    request: Some(Payload::Json("NotesWriteRequest")),
    responses: WRITE_RESPONSES,
};

/// How the resolution route is described in the generated `OpenAPI` document.
pub const RESOLUTION_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/notes/conflicts/{conflict_id}/resolution",
    operation_id: "resolveNotesConflict",
    summary: "Resolve a conflict of a connected vault",
    description: "\
Accepts the resolution of one open conflict durably and returns the operation of kind \
`notes.file.write` that tracks it. A resolution is one more write, `resolve_conflict`: it waits in \
the queue of its file and is checked like any other write. `choice` is `keep_current`, \
`keep_writer`, `accept_merge` (only for a conflict with a merge proposal) or `custom`, which names \
content uploaded first to `/v1/notes/connections/{connection_id}/blobs` in `blob`.\n\n\
The body names the connection of the conflict, as `GET /v1/notes/conflicts/{conflict_id}` gives it, \
because Edge holds no notes data. The notes service fails the operation with \
`notes.connection.not_found` when the caller does not own the connection or the connection does \
not hold the conflict.\n\n\
`Idempotency-Key` is required, as on the write route, and one key names one write across both \
routes: a key that a write took is refused here with `409`.",
    tag: "notes",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "conflict_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The open conflict.",
        },
        IDEMPOTENCY_PARAMETER,
    ],
    request: Some(Payload::Json("NotesResolutionRequest")),
    responses: WRITE_RESPONSES,
};

/// How the write-mode switch is described in the generated `OpenAPI` document.
pub const WRITE_MODE_DOC: RouteDoc = RouteDoc {
    method: Method::Put,
    path: "/v1/notes/connections/{connection_id}/write-mode",
    operation_id: "setNotesWriteMode",
    summary: "Turn write-back on or off for a connected vault",
    description: "\
Every connection starts with write mode off. Edge records an operation of kind \
`notes.connection.write_mode` and sends the request to the notes service with the operation id; the \
notes service reports the operation, because a `sync` connection must reconfigure its client before \
the mode is true. Turning write mode off fails every write that was not delivered yet.\n\n\
When the notes service does not take the request, Edge terminates the operation as `failed` with \
the code of the failure before it answers. `Idempotency-Key` is required: the same key and body \
return the ORIGINAL operation and are not sent to the notes service again.",
    tag: "notes",
    security: Security::Session,
    parameters: &[CONNECTION_PARAMETER, IDEMPOTENCY_PARAMETER],
    request: Some(Payload::Json("NotesWriteModeRequest")),
    responses: &[
        response(
            202,
            "The notes service took the request. The body carries the operation to poll.",
            "NotesWriteAccepted",
        ),
        response(
            400,
            "No `Idempotency-Key`, a connection id that is not a UUID, or a body other than \
             `{\"write_mode\":\"on\"}` or `{\"write_mode\":\"off\"}`. Or the notes service refused \
             the request, with its own code; the operation is then `failed`.",
            "ErrorEnvelope",
        ),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(
            403,
            "A device credential. A device calls only the device routes of notes.",
            "ErrorEnvelope",
        ),
        response(
            404,
            "The notes service does not know the connection for this caller \
             (`notes.connection.not_found`). The operation is `failed`.",
            "ErrorEnvelope",
        ),
        response(
            409,
            "The key is in use for a different request.",
            "ErrorEnvelope",
        ),
        response(
            413,
            "The body is larger than the `control` budget of the notes route.",
            "ErrorEnvelope",
        ),
        response(
            429,
            "This caller has spent its request allowance.",
            "ErrorEnvelope",
        ),
        response(
            502,
            "The notes service gave an answer that Edge cannot use. The operation is `failed`.",
            "ErrorEnvelope",
        ),
        response(
            503,
            "The notes service could not be reached. The operation is `failed`.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "The notes service did not answer within the `control` budget, or a dependency of Edge \
             did not answer in time.",
            "ErrorEnvelope",
        ),
    ],
};
