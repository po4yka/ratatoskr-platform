//! Anonymous sanitized status projection from cached observations.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{Method, Payload, ResponseDoc, RouteDoc, Security};
use ratatoskr_identifiers::WireTimestamp;
use ratatoskr_operational_contracts::{
    PublicComponentId, PublicComponentState, PublicStatusComponent, PublicStatusDocument,
    PublicStatusState,
};

use crate::ApiState;

/// Return public status without authenticating or probing a dependency.
pub async fn read(State(state): State<Arc<ApiState>>) -> Response {
    let generated_at = WireTimestamp::now();
    let api = PublicStatusComponent {
        id: PublicComponentId::Api,
        state: PublicComponentState::Operational,
        observed_at: None,
        stale: false,
    };
    let storage = dependency_component(
        PublicComponentId::Storage,
        state.health.database_reachable(),
        state.health.database_observed_at(),
    );
    let command_delivery = dependency_component(
        PublicComponentId::CommandDelivery,
        state.health.bus_reachable(),
        state.health.bus_observed_at(),
    );
    let connected_services = connected_services(&state).await;
    let components = vec![api, storage, command_delivery, connected_services];
    let overall = aggregate(&components);
    let document = match PublicStatusDocument::new(generated_at, overall, components) {
        Ok(document) => document,
        Err(error) => {
            tracing::error!(%error, "cached public status violated its contract");
            return platform_http::reject(platform_core::FailureKind::RequestTimeout);
        }
    };

    ([(http::header::CACHE_CONTROL, "no-store")], Json(document)).into_response()
}

fn dependency_component(
    id: PublicComponentId,
    reachable: Option<bool>,
    observed_at: Option<jiff::Timestamp>,
) -> PublicStatusComponent {
    let observed_at = observed_at.map(WireTimestamp::from_jiff);
    match reachable {
        Some(true) => PublicStatusComponent {
            id,
            state: PublicComponentState::Operational,
            observed_at,
            stale: false,
        },
        Some(false) => PublicStatusComponent {
            id,
            state: PublicComponentState::Unavailable,
            stale: observed_at.is_some(),
            observed_at,
        },
        None => PublicStatusComponent {
            id,
            state: PublicComponentState::Unknown,
            observed_at: None,
            stale: false,
        },
    }
}

async fn connected_services(state: &ApiState) -> PublicStatusComponent {
    let snapshots = state.gateway.capabilities().await;
    if snapshots.is_empty() {
        return PublicStatusComponent {
            id: PublicComponentId::ConnectedServices,
            state: PublicComponentState::Operational,
            observed_at: None,
            stale: false,
        };
    }

    let mut observations = Vec::with_capacity(snapshots.len());
    for snapshot in &snapshots {
        let Some(raw) = snapshot.observed_at.as_deref() else {
            return unknown_services();
        };
        let Ok(observed_at) = WireTimestamp::parse(raw) else {
            return unknown_services();
        };
        observations.push(observed_at);
    }
    let observed_at = observations.into_iter().min();
    let has_stale_observation = snapshots.iter().any(|snapshot| snapshot.stale);
    PublicStatusComponent {
        id: PublicComponentId::ConnectedServices,
        state: if has_stale_observation {
            PublicComponentState::Degraded
        } else {
            PublicComponentState::Operational
        },
        observed_at,
        stale: has_stale_observation,
    }
}

fn unknown_services() -> PublicStatusComponent {
    PublicStatusComponent {
        id: PublicComponentId::ConnectedServices,
        state: PublicComponentState::Unknown,
        observed_at: None,
        stale: false,
    }
}

fn aggregate(components: &[PublicStatusComponent]) -> PublicStatusState {
    if components.iter().any(|component| {
        matches!(
            component.id,
            PublicComponentId::Api | PublicComponentId::Storage
        ) && component.state == PublicComponentState::Unavailable
    }) {
        PublicStatusState::Unavailable
    } else if components
        .iter()
        .all(|component| component.state == PublicComponentState::Operational && !component.stale)
    {
        PublicStatusState::Operational
    } else {
        PublicStatusState::Degraded
    }
}

/// `OpenAPI` description for anonymous sanitized status.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/status",
    operation_id: "readPublicStatus",
    summary: "Read public status",
    description: "Projects four sanitized public component groups from cached observations. It performs no request-time dependency probes and is not the operator health surface.",
    tag: "status",
    security: Security::None,
    parameters: &[],
    request: None,
    responses: &[ResponseDoc {
        status: 200,
        description: "Current cached public status projection.",
        payload: Some(Payload::Json("PublicStatusDocument")),
    }],
};
