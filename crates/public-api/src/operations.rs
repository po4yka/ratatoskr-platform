//! Reading an operation.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use platform_core::FailureKind;
use uuid::Uuid;

use crate::{ApiState, Principal};

/// `GET /v2/operations/{operation_id}`.
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
