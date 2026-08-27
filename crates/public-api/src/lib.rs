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
use axum::routing::{MethodRouter, any, delete, get, post, put};
use platform_api_doc::{ApiSurface, RouteDoc};
use platform_http::RuntimeState;
use platform_persistence::Database;
use tower_http::limit::RequestBodyLimitLayer;

pub mod archives;
pub mod auth;
pub mod capabilities;
pub mod captures;
pub mod credentials;
pub mod devices;
pub mod gateway;
pub mod oauth;
pub mod operations;
pub mod sessions;
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
    /// The readiness facts, shared with `/health/ready`. `GET /v1/capabilities` reports whether a
    /// capability's dependency is healthy, and it must be the SAME fact readiness reports or one of
    /// the two answers is wrong (ADR-0008).
    pub health: Arc<RuntimeState>,
    /// Whether this deployment has an event bus. Not "is the broker up": a bus that is configured
    /// and briefly unreachable still publishes the outbox when it returns, whereas a deployment
    /// with none never will.
    pub bus_configured: bool,
    /// The Ed25519 PUBLIC key of the assertion issuer, decoded. `None` means this deployment does
    /// not accept identity assertions, and both the exchange route and the capability say so
    /// (ADR-0011).
    pub assertion_key: Option<Vec<u8>>,
    /// Where a browser goes after a provider callback has been relayed. Configured, never taken
    /// from the callback (ADR-0012).
    pub oauth_completion_url: Option<url::Url>,
    /// The per-actor allowance `ARCHITECTURE.md` S14 requires, applied in the [`Principal`]
    /// extractor — so every authenticated route is covered by one check rather than by a line
    /// somebody has to remember to add to the next handler.
    ///
    /// In memory, and that is correct rather than a compromise: exactly one edge process runs
    /// (ADR-0010), so this map is the whole system's view of an actor and not one replica's guess.
    pub actor_limit: Arc<platform_http::ActorLimiter>,
    /// A fixed pre-authentication budget for pairing attempts. The tunnel's client-address headers
    /// are attacker-controlled, so one process-wide bucket is the only honest identity before a
    /// pairing code has authenticated anything.
    pub pairing_limit: Arc<platform_http::ActorLimiter>,
    /// The configured loopback domain-service route table.
    pub gateway: gateway::Gateway,
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
            actor_limit: Arc::new(platform_http::ActorLimiter::new(
                platform_core::config::DEFAULT_ACTOR_REQUESTS_PER_MINUTE,
            )),
            pairing_limit: Arc::new(platform_http::ActorLimiter::new(20)),
            database,
            audience: audience.into(),
            idempotency_ttl: jiff::SignedDuration::from_hours(24),
            health,
            bus_configured,
            assertion_key: None,
            oauth_completion_url: None,
            gateway: gateway::Gateway::disabled(),
        }
    }

    /// What a capability is evaluated against.
    #[must_use]
    pub fn deployment(&self) -> platform_core::Deployment {
        platform_core::Deployment {
            database_reachable: self.health.database_reachable().unwrap_or(false),
            bus_configured: self.bus_configured,
            assertion_key_configured: self.assertion_key.is_some(),
        }
    }
}

/// The correlation the middleware already minted for this request (ADR-0007).
///
/// `Option` because a unit test may call a handler without the middleware; in production it is
/// always present. Shared by every handler that audits, so a record cannot be written with a
/// correlation the client never saw.
#[must_use]
pub fn correlation_of(context: Option<axum::Extension<platform_http::RequestContext>>) -> String {
    context.map_or_else(
        || platform_telemetry::correlation::mint_correlation().to_string(),
        |axum::Extension(context)| context.correlation_id.to_string(),
    )
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
/// Every path carries its major version. `ARCHITECTURE.md` S5.3: versioned `/v1` resource-oriented
/// routes. The version is 1, and it stays 1 while the project is in development: one system serves
/// this surface, and there is no second major to serve beside it.
fn table() -> Vec<Endpoint> {
    vec![
        Endpoint {
            doc: captures::DOC,
            handler: post(captures::submit),
        },
        Endpoint {
            doc: archives::PREPARE_DOC,
            handler: post(archives::prepare),
        },
        Endpoint {
            doc: archives::UPLOAD_DOC,
            handler: put(archives::upload),
        },
        Endpoint {
            doc: operations::DOC,
            handler: get(operations::read),
        },
        Endpoint {
            doc: operations::LIST_DOC,
            handler: get(operations::list),
        },
        Endpoint {
            doc: operations::CANCEL_DOC,
            handler: post(operations::cancel),
        },
        Endpoint {
            doc: stream::DOC,
            handler: get(stream::events),
        },
        Endpoint {
            doc: capabilities::DOC,
            handler: get(capabilities::read),
        },
        Endpoint {
            doc: sessions::DOC,
            handler: post(sessions::exchange_telegram),
        },
        Endpoint {
            doc: sessions::LIST_DOC,
            handler: get(sessions::list_sessions),
        },
        Endpoint {
            doc: sessions::REVOKE_DOC,
            handler: delete(sessions::revoke_session),
        },
        Endpoint {
            doc: sessions::REVOKE_ALL_DOC,
            handler: post(sessions::revoke_all),
        },
        Endpoint {
            doc: credentials::OPEN_DOC,
            handler: post(credentials::open_device_session),
        },
        Endpoint {
            doc: credentials::REFRESH_DOC,
            handler: post(credentials::refresh),
        },
        Endpoint {
            doc: devices::CREATE_CODE_DOC,
            handler: post(devices::create_pairing_code),
        },
        Endpoint {
            doc: devices::PAIR_DOC,
            handler: post(devices::pair),
        },
        Endpoint {
            doc: devices::LIST_DOC,
            handler: get(devices::list_devices),
        },
        Endpoint {
            doc: devices::DELETE_DOC,
            handler: delete(devices::delete_device),
        },
        Endpoint {
            doc: oauth::CALLBACK_DOC,
            handler: get(oauth::callback),
        },
        Endpoint {
            doc: oauth::CLAIM_DOC,
            handler: post(oauth::claim),
        },
    ]
}

/// The public routes.
pub fn routes(state: Arc<ApiState>) -> Router {
    let router = table().into_iter().fold(Router::new(), |router, endpoint| {
        router.route(endpoint.doc.path, endpoint.handler)
    });
    let upload_limit =
        usize::try_from(state.gateway.transfer_budget().max_body_bytes).unwrap_or(usize::MAX);
    let router = router.layer(RequestBodyLimitLayer::new(upload_limit));
    let router = state
        .gateway
        .routes()
        .values()
        .fold(router, |router, route| {
            let Some(budget) = state.gateway.budget(route) else {
                return router;
            };
            let limit = usize::try_from(budget.max_body_bytes).unwrap_or(usize::MAX);
            let handler = any(gateway::proxy).layer(RequestBodyLimitLayer::new(limit));
            router
                .route(&route.prefix, handler.clone())
                .route(&format!("{}/{{*tail}}", route.prefix), handler)
        });
    router.with_state(state)
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
    generator.subschema_for::<archives::PrepareArchive>();
    generator.subschema_for::<archives::ArchivePrepared>();
    generator.subschema_for::<capabilities::CapabilityDocument>();
    generator.subschema_for::<sessions::ExchangeAssertion>();
    generator.subschema_for::<sessions::SessionMinted>();
    generator.subschema_for::<sessions::SessionList>();
    generator.subschema_for::<sessions::RevokedAll>();
    generator.subschema_for::<credentials::OpenDeviceSession>();
    generator.subschema_for::<credentials::DeviceSessionOpened>();
    generator.subschema_for::<credentials::RefreshSession>();
    generator.subschema_for::<credentials::RotatedCredentials>();
    generator.subschema_for::<devices::CreatePairingCode>();
    generator.subschema_for::<devices::PairingCodeIssued>();
    generator.subschema_for::<devices::PairDevice>();
    generator.subschema_for::<devices::Paired>();
    generator.subschema_for::<devices::DeviceList>();
    generator.subschema_for::<oauth::RelayedCallback>();
    generator.subschema_for::<ratatoskr_operation_contracts::OperationSnapshot>();
    generator.subschema_for::<ratatoskr_error_contracts::ErrorEnvelope>();
    generator.subschema_for::<operations::OperationList>();
}
