//! Reading an operation.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use uuid::Uuid;

use crate::{ApiState, Principal};

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
