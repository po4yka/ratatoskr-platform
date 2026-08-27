//! What this deployment can actually do for the caller.
//!
//! `ARCHITECTURE.md` S12 and `AGENTS.md` rule 6: "Capabilities replace frontend assumptions." A
//! client asks instead of guessing which optional service is deployed, and gates its own features
//! on the answer.
//!
//! ADR-0008 fixes what the answer is computed from. Briefly: the vocabulary is closed
//! ([`platform_core::Capability`]) and holds only names this build serves a route for; a capability
//! is reported when the deployment has the components it needs, the last probe of those components
//! answered, and the caller is authorized for it.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{Method, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::Capability;

use crate::{ApiState, Principal};

/// The major version of this surface, as a client reads it. `ARCHITECTURE.md` S12.
///
/// A build constant, not configuration: it states what this binary serves, and an operator does not
/// decide that.
const API_VERSION: &str = "1.0";

/// The oldest client release this surface still answers correctly.
///
/// Constants for the same reason [`API_VERSION`] is one. Raising a floor is a decision about the
/// API's behaviour — a removed field, a narrowed accept — and it is made in the pull request that
/// makes that change, not in a `ConfigMap`.
const MINIMUM_WEB: &str = "1.0";
const MINIMUM_MOBILE: &str = "1.0";

/// What a client is told it may do.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct CapabilityDocument {
    /// The major version of this API surface.
    pub api_version: &'static str,
    /// The oldest client release per surface that this API still answers correctly.
    pub minimum_client_versions: MinimumClientVersions,
    /// The capabilities available to the caller right now, sorted, so two consecutive responses
    /// from an unchanged deployment are byte-identical.
    pub capabilities: Vec<&'static str>,
    /// Service-owned capability documents last sampled by Edge. Each section states whether it is
    /// stale instead of fabricating an empty success when its loopback owner is absent.
    pub services: Vec<crate::gateway::ServiceCapabilities>,
}

/// The client-version floors.
///
/// A struct rather than a map, so the set of surfaces is closed and a typo in a client name cannot
/// silently produce a floor nobody reads. `ARCHITECTURE.md` S12 names these two.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct MinimumClientVersions {
    /// `ratatoskr-web`.
    pub web: &'static str,
    /// `ratatoskr-mobile`.
    pub mobile: &'static str,
}

/// Publish `platform_capability_available{capability}` from the same facts the route reports.
///
/// Called on a timer by the process that owns the state, not from the handler. A capability is a
/// pure function of the deployment (ADR-0008), so its value is knowable whether or not a client
/// asks — and a gauge that only moves when somebody asks reports the state of the last question
/// rather than the state of the deployment. That distinction is the whole reason
/// `DEVELOPMENT.md`'s S16 table calls a metric published from wherever was convenient "exactly the
/// misleading series" it refuses.
///
/// Every capability is published on every tick, including the unavailable ones, so a series does
/// not vanish from a dashboard at the moment the thing it watches breaks.
pub fn sample(state: &ApiState) {
    let deployment = state.deployment();
    for capability in Capability::ALL {
        let available = capability.requires().is_met(&deployment);
        metrics::gauge!(
            platform_telemetry::metrics::PLATFORM_CAPABILITY_AVAILABLE,
            "capability" => capability.as_str(),
        )
        .set(f64::from(u8::from(available)));
    }
}

/// `GET /v1/capabilities`.
///
/// Authenticated, like every other `/v1` route, for the three reasons ADR-0008 records: the
/// authorization input is per-principal by definition, the health input is operational state that
/// `ARCHITECTURE.md` S15 keeps off an anonymous surface, and an authenticated route can be opened
/// to anonymous callers later while the reverse breaks every client.
pub async fn read(
    State(state): State<Arc<ApiState>>,
    // Authenticates, and is then deliberately unused: no capability in the vocabulary is
    // grant-gated yet, so the document is the same for every principal. The filter over
    // `identity.grants` arrives with the first capability that needs one, and the route already
    // authenticates, so that change is invisible to a client.
    _principal: Principal,
) -> Response {
    // What this deployment has, from the same facts readiness reports — never a fresh probe.
    let deployment = state.deployment();

    let capabilities = Capability::ALL
        .into_iter()
        .filter(|capability| capability.requires().is_met(&deployment))
        .map(Capability::as_str)
        .collect();

    let services = state.gateway.capabilities().await;
    (
        http::StatusCode::OK,
        Json(CapabilityDocument {
            api_version: API_VERSION,
            minimum_client_versions: MinimumClientVersions {
                web: MINIMUM_WEB,
                mobile: MINIMUM_MOBILE,
            },
            capabilities,
            services,
        }),
    )
        .into_response()
}

/// How this route is described in the generated `OpenAPI` document.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/capabilities",
    operation_id: "readCapabilities",
    summary: "What this deployment can do for you",
    description: "\
Returns the API version, the client-version floors, and the capabilities available to the caller.\n\n\
A capability is a stable name a client gates a feature on. It appears when this deployment has the \
components that capability needs, those components answered their last health probe, and the \
caller is authorized for it. It disappears the moment any of the three stops holding, so a client \
must read this on every session rather than caching it across deployments.\n\n\
The array is sorted and its vocabulary is closed: a name that is absent is a name this deployment \
cannot honour, never a name it forgot to mention. Treat an unfamiliar name as a feature this \
client does not implement, and a familiar name that is absent as a feature to hide.",
    tag: "capabilities",
    security: Security::Session,
    parameters: &[],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The capability document.",
            payload: Some(Payload::Json("CapabilityDocument")),
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
    ],
};
