//! Reading an operation, asking for one to stop, and listing an owner's operations.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_eventing::{MessageClass, Outbox, Subject};
use uuid::Uuid;

use platform_identity::audit::{self, AuditEvent, AuditOutcome};

use crate::{ApiState, Principal};

/// The command a cancellation acceptance publishes. The owning service is its consumer; the
/// subject mirrors the platform-scoped event family that reports outcomes back.
const CANCEL_COMMAND_TYPE: &str = "platform.operation.cancel_requested.v1";

/// What the audit trail calls an accepted cancellation request.
const CANCEL_ACTION: &str = "platform.operation.cancel_requested";

/// `GET /v1/operations/{operation_id}`.
///
/// Returns the contract `OperationSnapshot`, which is the same shape every other consumer of an
/// operation sees. Platform does not define a second, API-only projection: `INTERFACES.md` requires
/// public routes to use the generated contract, and a parallel shape would drift.
///
/// An operation belonging to another principal produces the same 404 as one that does not exist.
/// `ARCHITECTURE.md` S15: authorization before existence is disclosed.
pub async fn read(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(operation_id): Path<Uuid>,
) -> Response {
    let pool = state.database.pool();

    let operation = match platform_operations::find(pool, operation_id).await {
        Ok(Some(operation)) => operation,
        Ok(None) => return platform_http::reject(FailureKind::NotFound),
        Err(error) => {
            tracing::error!(%error, "the operation could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    if operation.owner_user_id != principal.user_id {
        // Deliberately the same refusal as "no such operation".
        return platform_http::reject(FailureKind::NotFound);
    }

    match platform_operations::snapshot(pool, operation_id).await {
        Ok(snapshot) => (http::StatusCode::OK, Json(snapshot)).into_response(),
        Err(error) => {
            // A stored row that cannot be projected is a defect in whatever wrote it, and it must
            // not reach a client as a malformed body.
            tracing::error!(%error, "the operation could not be projected");
            platform_http::reject(FailureKind::RequestTimeout)
        }
    }
}

/// The query string of `GET /v1/operations`.
///
/// Every member arrives as its wire text and is validated here rather than through typed
/// deserialization, because each refusal is a documented client error with one envelope code -
/// which is exactly what the unauthored-400 path already produces for a malformed query string.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ListParams {
    /// Restrict to one lifecycle status.
    pub state: Option<String>,
    /// Restrict to one exact kind.
    pub kind: Option<String>,
    /// How many rows to answer with, bounded below by 1 and above by [`MAX_PAGE_SIZE`].
    pub limit: Option<String>,
    /// The continuation cursor from the previous response.
    pub cursor: Option<String>,
}

/// The page size a client gets when it asks for none.
const DEFAULT_PAGE_SIZE: i64 = 20;

/// The largest page a client may ask for, matching the retired monolith's list bounds.
const MAX_PAGE_SIZE: i64 = 100;

/// One listed operation: identification and lifecycle truth, no payloads.
///
/// Result references, errors and warnings stay the singular endpoint's job; a listing answers
/// "what is the state of my work", and a client that wants an outcome follows the identifier.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct OperationSummary {
    /// The operation's identity.
    pub operation_id: Uuid,
    /// What work it performs.
    pub kind: String,
    /// Where it is in the lifecycle.
    pub status: ratatoskr_operation_contracts::OperationStatus,
    /// A producer-defined display phase, when one was reported.
    pub stage: Option<String>,
    /// Bounded progress, when one was reported.
    pub progress_percent: Option<i16>,
    /// Whether the client may resubmit this work after a failure.
    pub retryable: bool,
    /// The correlation identifier that ties this operation to its acceptance request.
    pub correlation_id: String,
    /// When it was accepted.
    pub accepted_at: ratatoskr_identifiers::WireTimestamp,
    /// When its status last changed.
    pub status_changed_at: ratatoskr_identifiers::WireTimestamp,
    /// When it finished, if it has.
    pub terminated_at: Option<ratatoskr_identifiers::WireTimestamp>,
}

/// One page of [`OperationSummary`] rows, newest accepted first.
///
/// `next_cursor` is always present: `null` means the walk is finished, a string means at least
/// one more page exists. A cursor is opaque to the client - pass it back verbatim.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct OperationList {
    /// The rows of this page.
    pub operations: Vec<OperationSummary>,
    /// The continuation point, or `null` when this page holds the end of the listing.
    pub next_cursor: Option<String>,
}

/// `GET /v1/operations`.
///
/// Lists the authenticated owner's operations, newest accepted first. Filters combine by
/// conjunction; pagination walks a keyset anchored on `(accepted_at, operation_id)`, so pages
/// never shift under concurrent inserts and no offset arithmetic exists on the path.
pub async fn list(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    axum::extract::Query(params): axum::extract::Query<ListParams>,
) -> Response {
    let scope_status = match params.state.as_deref() {
        None => None,
        Some(raw) => match parse_state(raw) {
            Some(status) => Some(status),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };
    let scope_kind = match params.kind.as_deref() {
        None => None,
        Some(raw) if is_listable_kind(raw) => Some(raw),
        Some(_) => return platform_http::reject(FailureKind::InvalidRequest),
    };
    let limit = match params.limit.as_deref() {
        None => DEFAULT_PAGE_SIZE,
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if (1..=MAX_PAGE_SIZE).contains(&value) => value,
            _ => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };
    let before = match params.cursor.as_deref() {
        None | Some("") => None,
        Some(raw) => match decode_cursor(raw) {
            Some(anchor) => Some(anchor),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };

    let page = match platform_operations::list_operations(
        state.database.pool(),
        platform_operations::ListScope {
            owner_user_id: principal.user_id,
            status: scope_status,
            kind: scope_kind,
            before,
            limit,
        },
    )
    .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "the operation listing could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let next_cursor = page
        .has_more
        .then_some(page.rows.last())
        .flatten()
        .map(|last| encode_cursor(last.accepted_at, last.operation_id));
    let operations = page.rows.iter().map(OperationSummary::of).collect();

    (
        http::StatusCode::OK,
        Json(OperationList {
            operations,
            next_cursor,
        }),
    )
        .into_response()
}

impl OperationSummary {
    fn of(operation: &platform_operations::Operation) -> Self {
        Self {
            operation_id: operation.operation_id,
            kind: operation.kind.clone(),
            status: operation.status,
            stage: operation.stage.clone(),
            progress_percent: operation.progress_percent,
            retryable: operation.retryable,
            correlation_id: operation.correlation_id.clone(),
            accepted_at: ratatoskr_identifiers::WireTimestamp::from_jiff(operation.accepted_at),
            status_changed_at: ratatoskr_identifiers::WireTimestamp::from_jiff(
                operation.status_changed_at,
            ),
            terminated_at: operation
                .terminated_at
                .map(ratatoskr_identifiers::WireTimestamp::from_jiff),
        }
    }
}

/// Parse a `state` filter value against the closed status vocabulary.
fn parse_state(raw: &str) -> Option<ratatoskr_operation_contracts::OperationStatus> {
    serde_json::from_value(serde_json::Value::String(raw.to_owned())).ok()
}

/// Whether a kind filter names a shape the schema would accept, so a typo is a refusal rather
/// than a silently empty page. The rule mirrors `schema.sql`'s `operations_kind_is_bounded`.
fn is_listable_kind(raw: &str) -> bool {
    let segments: Vec<&str> = raw.split('.').collect();
    (2..=4).contains(&segments.len())
        && segments.iter().all(|segment| {
            let mut characters = segment.chars();
            matches!(characters.next(), Some(first) if first.is_ascii_lowercase())
                && characters.count() <= 31
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// The continuation anchor of one row, as opaque text: acceptance instant then identity.
fn encode_cursor(accepted_at: jiff::Timestamp, operation_id: Uuid) -> String {
    format!("{}.{}", accepted_at.as_microsecond(), operation_id)
}

/// Decode a continuation cursor, refusing anything that is not one this service could have minted.
///
/// A forged but well-formed value can only move the window within the caller's own listing, which
/// any legitimate cursor does too; there is no secret to leak and nothing to sign.
fn decode_cursor(raw: &str) -> Option<(jiff::Timestamp, Uuid)> {
    let (micros, identifier) = raw.split_once('.')?;
    let operation_id = Uuid::parse_str(identifier).ok()?;
    let accepted_at = jiff::Timestamp::from_microsecond(micros.parse::<i64>().ok()?).ok()?;
    Some((accepted_at, operation_id))
}

/// How the listing route is described in the generated `OpenAPI` document.
pub const LIST_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/operations",
    operation_id: "listOperations",
    summary: "List your operations",
    description: "\
Answers with your operations, newest accepted first, in pages you size. Filter them by lifecycle \
`state`, by exact `kind`, or both at once; follow `next_cursor` for the rest. The cursor is \
opaque: pages cannot shift while you walk them, and passing `null` back means you are done.\n\n\
Rows carry identification and lifecycle facts but not result references, errors or warnings - \
read those from `GET /v1/operations/{operation_id}`, which stays the place an outcome lives.",
    tag: "operations",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "state",
            location: In::Query,
            required: false,
            format: None,
            description: "Only operations in this lifecycle state. One of accepted, queued, \
                          running, succeeded, partially_succeeded, failed, cancelled.",
        },
        Parameter {
            name: "kind",
            location: In::Query,
            required: false,
            format: None,
            description: "Only operations of exactly this kind, e.g. content.capture.submit.",
        },
        Parameter {
            name: "limit",
            location: In::Query,
            required: false,
            format: None,
            description: "How many rows to answer with, 1 to 100. The default is 20.",
        },
        Parameter {
            name: "cursor",
            location: In::Query,
            required: false,
            format: None,
            description: "The `next_cursor` value of the previous response, verbatim.",
        },
    ],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "One page. `next_cursor` is null after the last one.",
            payload: Some(Payload::Json("OperationList")),
        },
        ResponseDoc {
            status: 400,
            description: "A state outside the vocabulary, a malformed kind, a page size outside \
                          1..=100, or a cursor this service did not issue.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "This caller has spent its request allowance. Retryable: the allowance \
                          refills continuously, so waiting is the fix.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How this route is described in the generated `OpenAPI` document.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/operations/{operation_id}",
    operation_id: "readOperation",
    summary: "The current state of one operation",
    description: "\
Returns the operation as a snapshot: its status, the stage it is in, its progress, and its result \
or its errors once it has one. The same shape every consumer of an operation sees; there is no \
second, API-only projection to keep in step.\n\n\
Poll this, or subscribe to `GET /v1/operations/{operation_id}/events` to be told instead of \
asking. An operation you do not own and an operation that does not exist both answer 404, so this \
route cannot be used to discover which identifiers are real.",
    tag: "operations",
    security: Security::Session,
    parameters: &[Parameter {
        name: "operation_id",
        location: In::Path,
        required: true,
        format: Some("uuid"),
        description: "The operation, as returned by the route that created it.",
    }],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The snapshot.",
            payload: Some(Payload::Json("OperationSnapshot")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "This caller has spent its request allowance. Retryable: the allowance \
                          refills continuously, so waiting is the fix.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "No such operation, or it belongs to somebody else.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// `POST /v1/operations/{operation_id}/cancel`.
///
/// Asks the owning service to stop, and answers with truth. A live operation records the
/// request — the schema's `cancellation_requested_at`, "a request, not a state" — and enqueues one
/// cancellation command in the same transaction; the operation reaches `cancelled` only when that
/// service reports it back through the same progress contract as any other outcome. A finished
/// operation answers with its current snapshot untouched: cancelling what already ended is never
/// an error, and needs no command.
///
/// The endpoint takes no `Idempotency-Key` on purpose. The operation identifier is the
/// idempotency domain: the guarded write behind this route makes a repeat answer with current
/// truth and enqueue nothing further, so a key would add client ceremony without adding safety.
///
/// An operation belonging to another principal produces the same 404 as one that does not exist.
pub async fn cancel(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(operation_id): Path<Uuid>,
) -> Response {
    let now = jiff::Timestamp::now();

    let Ok(subject) = Subject::new(MessageClass::Command, CANCEL_COMMAND_TYPE) else {
        tracing::error!(
            command = CANCEL_COMMAND_TYPE,
            "the command subject is not constructible"
        );
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let outcome = match platform_operations::request_cancellation(
        &mut transaction,
        operation_id,
        principal.user_id,
        now,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(platform_operations::OperationError::NotFound) => {
            // A missing identifier and somebody else's are the same refusal (ARCHITECTURE S15).
            // Dropping the transaction writes nothing.
            return platform_http::reject(FailureKind::NotFound);
        }
        Err(error) => {
            tracing::error!(%error, "the cancellation could not be recorded");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    if let platform_operations::Cancellation::Requested(recorded) = &outcome {
        // One command per request recorded. The idempotency key IS the operation: a consumer that
        // somehow sees two of these can tell they name the same stop request.
        //
        // Correlation is the operation's OWN identifier, not this request's: one operation lives
        // on one correlation thread from its acceptance command through every report, and the
        // audit record below joins that same string.
        let payload = platform_eventing::Command {
            command_type: CANCEL_COMMAND_TYPE,
            operation_id,
            principal: principal.user_id,
            correlation_id: &recorded.correlation_id,
            idempotency_key: &operation_id.to_string(),
            requested_at: now,
        }
        .envelope(serde_json::json!({ "requested_at": now.to_string() }));

        if let Err(error) = Outbox::enqueue(
            &mut *transaction,
            Uuid::now_v7(),
            &subject,
            &payload,
            Some(operation_id),
            now,
        )
        .await
        {
            tracing::error!(%error, "the cancellation command could not be enqueued");
            return platform_http::reject(FailureKind::RequestTimeout);
        }

        let event = AuditEvent {
            audit_event_id: Uuid::now_v7(),
            actor_user_id: Some(principal.user_id),
            actor_session_id: Some(principal.session_id),
            action: CANCEL_ACTION,
            target_kind: "operation",
            target_id: Some(operation_id),
            outcome: AuditOutcome::Allowed,
            correlation_id: recorded.correlation_id.clone(),
        };
        if let Err(error) = audit::record(&mut *transaction, &event, now).await {
            tracing::error!(%error, "the cancellation could not be audited");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    }

    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the cancellation transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    truth(pool, operation_id).await
}

/// The post-commit answer: current truth, `200` once the outcome is fixed, `202` while it is not.
///
/// Read AFTER the commit rather than from the locked row, so the status code reflects the state
/// the caller will see when they look again instead of the one the lock happened to find.
async fn truth(pool: &sqlx::PgPool, operation_id: Uuid) -> Response {
    let snapshot = match platform_operations::snapshot(pool, operation_id).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::error!(%error, "the operation could not be projected");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let code = if platform_operations::transition::is_terminal(snapshot.status) {
        http::StatusCode::OK
    } else {
        http::StatusCode::ACCEPTED
    };
    (code, Json(snapshot)).into_response()
}

/// How the cancel route is described in the generated `OpenAPI` document.
pub const CANCEL_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/operations/{operation_id}/cancel",
    operation_id: "cancelOperation",
    summary: "Ask for an operation to stop",
    description: "\
Records a request to stop one of your operations and delivers it to the service executing the \
work, which stops cooperatively and reports the outcome like any other progress report. The \
response carries the operation's CURRENT state, never the requested one: between the request and \
the confirmation the operation keeps whatever status its work has actually reached.\n\n\
The route is idempotent without an `Idempotency-Key`. Cancelling twice queues one stop request, \
and cancelling an operation that already finished answers with its outcome as plain truth - a \
`succeeded` capture stays `succeeded`.",
    tag: "operations",
    security: Security::Session,
    parameters: &[Parameter {
        name: "operation_id",
        location: In::Path,
        required: true,
        format: Some("uuid"),
        description: "The operation to stop, as returned by the route that created it.",
    }],
    request: None,
    responses: &[
        ResponseDoc {
            status: 202,
            description: "The stop request was recorded for a still-running operation. The body \
                          carries its current snapshot.",
            payload: Some(Payload::Json("OperationSnapshot")),
        },
        ResponseDoc {
            status: 200,
            description: "Nothing to stop: the operation had already finished, and the body \
                          carries its outcome unchanged.",
            payload: Some(Payload::Json("OperationSnapshot")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "This caller has spent its request allowance. Retryable: the allowance \
                          refills continuously, so waiting is the fix.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "No such operation, or it belongs to somebody else.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written; retrying is \
                          safe and converges on current truth.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
