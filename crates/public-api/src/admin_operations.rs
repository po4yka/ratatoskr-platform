//! Owner-authorized operation inspection routes.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use ratatoskr_error_contracts::ErrorCode;
use ratatoskr_identifiers::{OperationId, UserId, WireTimestamp};
use ratatoskr_operation_contracts::{OperationKind, OperationStatus};
use ratatoskr_operational_contracts::{
    InspectionCursor, OperationInspectionPage, OperationInspectionSummary,
};
use uuid::Uuid;

use crate::{ApiState, Principal};

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 100;

/// Filters accepted by the deployment-wide operation listing.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ListParams {
    /// Exact lifecycle state.
    pub state: Option<String>,
    /// Exact operation kind.
    pub kind: Option<String>,
    /// Exact owner identity.
    pub owner_user_id: Option<String>,
    /// Page size from 1 through 100.
    pub limit: Option<String>,
    /// Opaque cursor from the previous page.
    pub cursor: Option<String>,
}

/// List operations across users after a live owner check.
pub async fn list(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Query(params): Query<ListParams>,
) -> Response {
    if let Err(response) = crate::admin::require_owner(&state, principal).await {
        return response;
    }

    let status = match params.state.as_deref() {
        None => None,
        Some(raw) => match parse_state(raw) {
            Some(value) => Some(value),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };
    let kind = match params.kind.as_deref() {
        None => None,
        Some(raw) if is_listable_kind(raw) => Some(raw),
        Some(_) => return platform_http::reject(FailureKind::InvalidRequest),
    };
    let owner_user_id = match params.owner_user_id.as_deref() {
        None => None,
        Some(raw) => match canonical_uuid(raw) {
            Some(value) => Some(value),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
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
            Some(value) => Some(value),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };

    let page = match platform_operations::list_admin_operations(
        state.database.pool(),
        platform_operations::AdminListScope {
            owner_user_id,
            status,
            kind,
            before,
            limit,
        },
    )
    .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "owner operation inspection could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let next_cursor = page
        .has_more
        .then_some(page.rows.last())
        .flatten()
        .and_then(|last| encode_cursor(last.operation.accepted_at, last.operation.operation_id));
    let items = match page.rows.iter().map(summary).collect::<Result<Vec<_>, _>>() {
        Ok(items) => items,
        Err(error) => {
            tracing::error!(%error, "stored operation could not become an inspection summary");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let response = match OperationInspectionPage::new(items, next_cursor) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "bounded operation page violated its contract");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    (http::StatusCode::OK, Json(response)).into_response()
}

/// Read one operation regardless of its owner after a live owner check.
pub async fn read(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(operation_id): Path<Uuid>,
) -> Response {
    if let Err(response) = crate::admin::require_owner(&state, principal).await {
        return response;
    }

    match platform_operations::snapshot(state.database.pool(), operation_id).await {
        Ok(snapshot) => (http::StatusCode::OK, Json(snapshot)).into_response(),
        Err(platform_operations::OperationError::NotFound) => {
            platform_http::reject(FailureKind::NotFound)
        }
        Err(error) => {
            tracing::error!(%error, "owner operation detail could not be projected");
            platform_http::reject(FailureKind::RequestTimeout)
        }
    }
}

fn summary(
    inspected: &platform_operations::InspectedOperation,
) -> Result<OperationInspectionSummary, String> {
    let operation = &inspected.operation;
    Ok(OperationInspectionSummary {
        operation_id: OperationId(operation.operation_id),
        owner_user_id: UserId(operation.owner_user_id),
        kind: OperationKind::parse(&operation.kind).map_err(|error| error.to_string())?,
        status: operation.status,
        accepted_at: WireTimestamp::from_jiff(operation.accepted_at),
        status_changed_at: WireTimestamp::from_jiff(operation.status_changed_at),
        failure_code: inspected
            .failure_code
            .as_deref()
            .map(ErrorCode::parse)
            .transpose()
            .map_err(|error| error.to_string())?,
    })
}

fn parse_state(raw: &str) -> Option<OperationStatus> {
    serde_json::from_value(serde_json::Value::String(raw.to_owned())).ok()
}

fn is_listable_kind(raw: &str) -> bool {
    OperationKind::parse(raw).is_ok()
}

fn canonical_uuid(raw: &str) -> Option<Uuid> {
    let value = Uuid::parse_str(raw).ok()?;
    (value.to_string() == raw).then_some(value)
}

fn encode_cursor(accepted_at: jiff::Timestamp, operation_id: Uuid) -> Option<InspectionCursor> {
    InspectionCursor::parse(&format!(
        "{}_{}",
        accepted_at.as_microsecond(),
        operation_id
    ))
    .ok()
}

fn decode_cursor(raw: &str) -> Option<(jiff::Timestamp, Uuid)> {
    InspectionCursor::parse(raw).ok()?;
    let (micros, identifier) = raw.split_once('_')?;
    let accepted_at = jiff::Timestamp::from_microsecond(micros.parse::<i64>().ok()?).ok()?;
    let operation_id = canonical_uuid(identifier)?;
    Some((accepted_at, operation_id))
}

/// `OpenAPI` description for the bounded owner operation list.
pub const LIST_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/admin/operations",
    operation_id: "inspectOperations",
    summary: "Inspect recent operations",
    description: "Returns a bounded newest-first page across users after a live owner grant check. Rows contain lifecycle facts and a stable safe failure code only.",
    tag: "administration",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "state",
            location: In::Query,
            required: false,
            format: None,
            description: "Exact operation lifecycle state.",
        },
        Parameter {
            name: "kind",
            location: In::Query,
            required: false,
            format: None,
            description: "Exact operation kind.",
        },
        Parameter {
            name: "owner_user_id",
            location: In::Query,
            required: false,
            format: Some("uuid"),
            description: "Exact operation owner.",
        },
        Parameter {
            name: "limit",
            location: In::Query,
            required: false,
            format: None,
            description: "Page size from 1 through 100; defaults to 20.",
        },
        Parameter {
            name: "cursor",
            location: In::Query,
            required: false,
            format: None,
            description: "Opaque continuation cursor from the previous page.",
        },
    ],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "One bounded inspection page.",
            payload: Some(Payload::Json("OperationInspectionPage")),
        },
        ResponseDoc {
            status: 400,
            description: "A filter, page size, owner identifier, or cursor is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No valid session credential.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 403,
            description: "The authenticated user does not hold the live owner grant.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "The caller has spent its request allowance.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "Authorization or operation storage could not answer.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// `OpenAPI` description for owner operation detail.
pub const READ_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/admin/operations/{operation_id}",
    operation_id: "inspectOperation",
    summary: "Inspect one operation",
    description: "Returns the existing operation snapshot after a live owner grant check.",
    tag: "administration",
    security: Security::Session,
    parameters: &[Parameter {
        name: "operation_id",
        location: In::Path,
        required: true,
        format: Some("uuid"),
        description: "The operation to inspect.",
    }],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The operation snapshot.",
            payload: Some(Payload::Json("OperationSnapshot")),
        },
        ResponseDoc {
            status: 401,
            description: "No valid session credential.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 403,
            description: "The authenticated user does not hold the live owner grant.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "No such operation.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "The caller has spent its request allowance.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "Authorization or operation storage could not answer.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
