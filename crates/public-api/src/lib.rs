//! The versioned public API.
//!
//! Milestone 5, and the first thing in this repository that a client can call. It is where the four
//! earlier milestones meet: a request authenticates against `identity`, reserves an idempotency key,
//! creates an `operations` record and enqueues a command into the outbox — the last three in ONE
//! transaction, which is the whole point of building them that way.
//!
//! Two rules govern every handler here.
//!
//! **A handler never authors an error body.** It returns [`platform_http::reject`] with a named
//! [`FailureKind`], and the one middleware renders the envelope. There is still exactly one
//! `ErrorEnvelope` construction site in the repository (test F-1).
//!
//! **Authorization comes before existence.** `ARCHITECTURE.md` S15. An operation that belongs to
//! someone else and an operation that does not exist produce the same 404, so the API is not an
//! oracle for which identifiers are real.

use std::sync::Arc;

use axum::Router;
use axum::routing::{MethodRouter, get, post};
use platform_api_doc::{ApiSurface, RouteDoc};
use platform_http::RuntimeState;
use platform_persistence::Database;

pub mod auth;
pub mod capabilities;
pub mod captures;
pub mod operations;
pub mod stream;

pub use crate::auth::Principal;

/// What every handler needs.
#[derive(Debug, Clone)]
pub struct ApiState {
    /// The pool. `identity` and `operations` only.
    pub database: Database,
    /// The audience this listener serves. A session issued for another audience does not
    /// authenticate here, which is what stops a token minted for one surface being replayed at
    /// another (`SECURITY.md`: validate issuer/audience/expiry/nonce).
    pub audience: String,
    /// How long an idempotency key is honoured.
    pub idempotency_ttl: jiff::SignedDuration,
    /// The readiness facts, shared with `/health/ready`. `GET /v2/capabilities` reports whether a
    /// capability's dependency is healthy, and it must be the SAME fact readiness reports or one of
    /// the two answers is wrong (ADR-0008).
    pub health: Arc<RuntimeState>,
    /// Whether this deployment has an event bus. Not "is the broker up": a bus that is configured
    /// and briefly unreachable still publishes the outbox when it returns, whereas a deployment
    /// with none never will.
    pub bus_configured: bool,
}

impl ApiState {
    /// The default replay window: 24 hours.
    ///
    /// Long enough to cover a client retrying after an outage, short enough that the ledger is not a
    /// permanent record of every request ever made.
    #[must_use]
    pub fn new(
        database: Database,
        audience: impl Into<String>,
        health: Arc<RuntimeState>,
        bus_configured: bool,
    ) -> Self {
        Self {
            database,
            audience: audience.into(),
            idempotency_ttl: jiff::SignedDuration::from_hours(24),
            health,
            bus_configured,
        }
    }
}

/// One served route: how it is reached, and how it is described.
///
/// The pair is the whole anti-drift mechanism (ADR-0006). [`routes`] folds the first half into a
/// router and [`surface`] collects the second into the `OpenAPI` document, from ONE list — so a
/// documented route with no handler does not compile, and a served route with no description
/// cannot be added without writing one.
struct Endpoint {
    doc: RouteDoc,
    handler: MethodRouter<Arc<ApiState>>,
}

/// Every route `ratatoskr-edge` serves.
///
/// Every path carries its major version. `ARCHITECTURE.md` S5.3: versioned `/v2` resource-oriented
/// routes. The version is 2 because Ratatoskr Next is the second system to serve this surface, and
/// starting at `/v1` would collide with the retired one in any client that ever spoke to both.
fn table() -> Vec<Endpoint> {
    vec![
        Endpoint {
            doc: captures::DOC,
            handler: post(captures::submit),
        },
        Endpoint {
            doc: operations::DOC,
            handler: get(operations::read),
        },
        Endpoint {
            doc: stream::DOC,
            handler: get(stream::events),
        },
        Endpoint {
            doc: capabilities::DOC,
            handler: get(capabilities::read),
        },
    ]
}

/// The public routes.
pub fn routes(state: ApiState) -> Router {
    let state = Arc::new(state);
    table()
        .into_iter()
        .fold(Router::new(), |router, endpoint| {
            router.route(endpoint.doc.path, endpoint.handler)
        })
        .with_state(state)
}

/// This listener's half of the generated `OpenAPI` document.
#[must_use]
pub fn surface() -> ApiSurface {
    ApiSurface {
        routes: table().into_iter().map(|endpoint| endpoint.doc).collect(),
        register: register_schemas,
    }
}

/// The payload types these routes carry.
///
/// Registering a type is what puts it in `components.schemas`; a route referring to one that is not
/// registered fails the document build rather than emitting a dangling reference.
fn register_schemas(generator: &mut schemars::SchemaGenerator) {
    generator.subschema_for::<captures::SubmitCapture>();
    generator.subschema_for::<captures::CaptureAccepted>();
    generator.subschema_for::<capabilities::CapabilityDocument>();
    generator.subschema_for::<ratatoskr_operation_contracts::OperationSnapshot>();
    generator.subschema_for::<ratatoskr_error_contracts::ErrorEnvelope>();
}
