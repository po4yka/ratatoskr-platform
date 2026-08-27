//! Owner-authorized schedule status inspection route.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use ratatoskr_identifiers::{UserId, WireTimestamp};
use ratatoskr_operation_contracts::OperationStatus;
use ratatoskr_operational_contracts::{
    InspectionCursor, ScheduleInspectionPage, ScheduleInspectionSummary, ScheduleName, ServiceName,
};
use uuid::Uuid;

use crate::{ApiState, Principal};

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 100;

/// Pagination accepted by schedule status inspection.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ListParams {
    /// Page size from 1 through 100.
    pub limit: Option<String>,
    /// Opaque cursor from the previous page.
    pub cursor: Option<String>,
}

/// List the deployment's Platform-owned schedule status projection.
pub async fn list(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Query(params): Query<ListParams>,
) -> Response {
    if let Err(response) = crate::admin::require_owner(&state, principal).await {
        return response;
    }

    let limit = match params.limit.as_deref() {
        None => DEFAULT_PAGE_SIZE,
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if (1..=MAX_PAGE_SIZE).contains(&value) => value,
            _ => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };
    let after = match params.cursor.as_deref() {
        None | Some("") => None,
        Some(raw) => match decode_cursor(raw) {
            Some(value) => Some(value),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };

    let page = match platform_scheduling::list_admin_schedules(
        state.database.pool(),
        platform_scheduling::AdminListScope { after, limit },
    )
    .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "owner schedule inspection could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let next_cursor = page
        .has_more
        .then_some(page.rows.last())
        .flatten()
        .and_then(|last| encode_cursor(last.next_due_at, last.schedule_id));
    let items = match page.rows.iter().map(summary).collect::<Result<Vec<_>, _>>() {
        Ok(items) => items,
        Err(error) => {
            tracing::error!(%error, "stored schedule could not become an inspection summary");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let response = match ScheduleInspectionPage::new(items, next_cursor) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "bounded schedule page violated its contract");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    (http::StatusCode::OK, Json(response)).into_response()
}

fn summary(
    inspected: &platform_scheduling::InspectedSchedule,
) -> Result<ScheduleInspectionSummary, String> {
    Ok(ScheduleInspectionSummary {
        schedule_id: inspected.schedule_id,
        service_name: ServiceName::parse(&inspected.service_name)
            .map_err(|error| error.to_string())?,
        name: ScheduleName::parse(&inspected.name).map_err(|error| error.to_string())?,
        owner_user_id: UserId(inspected.owner_user_id),
        next_due_at: WireTimestamp::from_jiff(inspected.next_due_at),
        enabled: inspected.enabled,
        last_outcome: inspected
            .last_outcome
            .as_deref()
            .map(parse_status)
            .transpose()?,
    })
}

fn parse_status(raw: &str) -> Result<OperationStatus, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_owned()))
        .map_err(|error| error.to_string())
}

fn encode_cursor(next_due_at: jiff::Timestamp, schedule_id: Uuid) -> Option<InspectionCursor> {
    InspectionCursor::parse(&format!("{}_{}", next_due_at.as_microsecond(), schedule_id)).ok()
}

fn decode_cursor(raw: &str) -> Option<(jiff::Timestamp, Uuid)> {
    InspectionCursor::parse(raw).ok()?;
    let (micros, identifier) = raw.split_once('_')?;
    let next_due_at = jiff::Timestamp::from_microsecond(micros.parse::<i64>().ok()?).ok()?;
    let schedule_id = Uuid::parse_str(identifier).ok()?;
    (schedule_id.to_string() == identifier).then_some((next_due_at, schedule_id))
}

/// `OpenAPI` description for the bounded owner schedule list.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/admin/schedules",
    operation_id: "inspectSchedules",
    summary: "Inspect schedule status",
    description: "Returns a bounded schedule-status page after a live owner grant check. Command payloads and schedule configuration are never selected.",
    tag: "administration",
    security: Security::Session,
    parameters: &[
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
            description: "One bounded schedule-status page.",
            payload: Some(Payload::Json("ScheduleInspectionPage")),
        },
        ResponseDoc {
            status: 400,
            description: "The page size or cursor is invalid.",
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
            description: "Authorization or schedule storage could not answer.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
