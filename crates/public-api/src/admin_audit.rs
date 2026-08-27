//! Owner-authorized audit trail inspection route.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use ratatoskr_identifiers::{EntityRef, UserId, WireTimestamp};
use ratatoskr_operational_contracts::{
    AuditAction, AuditEventPage, AuditEventSummary, AuditOutcome, AuditTargetKind, InspectionCursor,
};
use uuid::Uuid;

use crate::{ApiState, Principal};

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 100;

/// Pagination accepted by audit inspection.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ListParams {
    /// Page size from 1 through 100.
    pub limit: Option<String>,
    /// Opaque cursor from the previous page.
    pub cursor: Option<String>,
}

/// List the deployment's stable redacted audit records.
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
    let before = match params.cursor.as_deref() {
        None | Some("") => None,
        Some(raw) => match decode_cursor(raw) {
            Some(value) => Some(value),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };

    let page = match platform_identity::audit::list_admin_events(
        state.database.pool(),
        platform_identity::audit::AdminListScope { before, limit },
    )
    .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "owner audit inspection could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let next_cursor = page
        .has_more
        .then_some(page.rows.last())
        .flatten()
        .and_then(|last| encode_cursor(last.occurred_at, last.audit_event_id));
    let items = match page.rows.iter().map(summary).collect::<Result<Vec<_>, _>>() {
        Ok(items) => items,
        Err(error) => {
            tracing::error!(%error, "stored audit event could not become an inspection summary");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let response = match AuditEventPage::new(items, next_cursor) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "bounded audit page violated its contract");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    (http::StatusCode::OK, Json(response)).into_response()
}

fn summary(
    inspected: &platform_identity::audit::InspectedAuditEvent,
) -> Result<AuditEventSummary, String> {
    Ok(AuditEventSummary {
        audit_event_id: inspected.audit_event_id,
        occurred_at: WireTimestamp::from_jiff(inspected.occurred_at),
        actor_user_id: inspected.actor_user_id.map(UserId),
        actor_session_id: inspected.actor_session_id,
        action: AuditAction::parse(&inspected.action).map_err(|error| error.to_string())?,
        target_kind: AuditTargetKind::parse(&inspected.target_kind)
            .map_err(|error| error.to_string())?,
        target_id: inspected.target_id,
        outcome: parse_outcome(&inspected.outcome)?,
        correlation_id: EntityRef::parse(&inspected.correlation_id)
            .map_err(|error| error.to_string())?,
    })
}

fn parse_outcome(raw: &str) -> Result<AuditOutcome, String> {
    match raw {
        "allowed" => Ok(AuditOutcome::Allowed),
        "denied" => Ok(AuditOutcome::Denied),
        "failed" => Ok(AuditOutcome::Failed),
        _ => Err(format!("unknown audit outcome {raw}")),
    }
}

fn encode_cursor(occurred_at: jiff::Timestamp, event_id: Uuid) -> Option<InspectionCursor> {
    InspectionCursor::parse(&format!("{}_{}", occurred_at.as_microsecond(), event_id)).ok()
}

fn decode_cursor(raw: &str) -> Option<(jiff::Timestamp, Uuid)> {
    InspectionCursor::parse(raw).ok()?;
    let (micros, identifier) = raw.split_once('_')?;
    let occurred_at = jiff::Timestamp::from_microsecond(micros.parse::<i64>().ok()?).ok()?;
    let event_id = Uuid::parse_str(identifier).ok()?;
    (event_id.to_string() == identifier).then_some((occurred_at, event_id))
}

/// `OpenAPI` description for the bounded owner audit list.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/admin/audit-events",
    operation_id: "inspectAuditEvents",
    summary: "Inspect the audit trail",
    description: "Returns a bounded newest-first page after a live owner grant check. The fixed audit columns contain no request body, credential, or diagnostic payload.",
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
            description: "One bounded redacted audit page.",
            payload: Some(Payload::Json("AuditEventPage")),
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
            description: "Authorization or audit storage could not answer.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
