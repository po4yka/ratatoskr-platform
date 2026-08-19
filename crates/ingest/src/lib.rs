//! Generic ingress: what `ratatoskr-ingest` serves.
//!
//! `ARCHITECTURE.md` S9 gives this process six steps, and the whole crate is those six in order:
//!
//! 1. **source authentication** — [`source::authenticate`], a bearer credential stored as a digest;
//! 2. **receipt deduplication** — the idempotency ledger, scoped to the source (see [`webhook`]);
//! 3. **safe metadata normalization** — [`webhook::WebhookSignal`], a closed shape with bounds;
//! 4. **target bounded-context routing** — [`Target`], a closed list the `webhook_sources.target`
//!    column selects from;
//! 5. **command publication** — the same transactional outbox every other command goes through;
//! 6. **receipt status projection** — the operation the signal created, readable at
//!    `GET /v2/operations/{id}` by the user who owns the source.
//!
//! What it deliberately does not do, from the same section: fetch article bodies, run browsers,
//! summarize content, or hold a provider credential that belongs to a dedicated service. A signal
//! arrives, is bounded, and becomes a command. Everything after that is somebody else's process.
//!
//! Telegram is not here and never will be: it owns a bot identity, dialogue state, callbacks and
//! Mini App authentication, which is why `AGENTS.md` gives it a repository rather than an adapter.

use std::sync::Arc;

use axum::Router;
use axum::routing::{MethodRouter, post};
use platform_api_doc::{ApiSurface, RouteDoc};
use platform_persistence::{Database, PersistenceError};

pub mod source;
pub mod webhook;

pub use crate::source::{SourceError, WebhookSource, authenticate};

/// What every ingest handler needs.
///
/// No audience, unlike the client-facing listener: a webhook source presents a credential of its
/// own and has no session, so there is nothing for an audience to scope.
#[derive(Debug, Clone)]
pub struct IngestState {
    /// The pool. `platform_ingest` and `operations` only.
    pub database: Database,
    /// How long a source's event identifier is honoured as a deduplication key.
    pub idempotency_ttl: jiff::SignedDuration,
    /// The per-source allowance `ARCHITECTURE.md` S14 requires.
    ///
    /// Keyed by SOURCE and not by owner: two sources of one user are two independent senders, and a
    /// misbehaving one must not spend the allowance of the other. It is applied after the credential
    /// resolves, because before that there is no actor to charge.
    pub actor_limit: std::sync::Arc<platform_http::ActorLimiter>,
}

impl IngestState {
    /// The default replay window: 24 hours, the same as the client-facing surface.
    ///
    /// A provider that retries a webhook does so for hours, not days, and a window that outlived
    /// the retry policy would only keep rows nobody reads.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            actor_limit: std::sync::Arc::new(platform_http::ActorLimiter::new(
                platform_core::config::DEFAULT_ACTOR_REQUESTS_PER_MINUTE,
            )),
            database,
            idempotency_ttl: jiff::SignedDuration::from_hours(24),
        }
    }
}

/// Where a source's signals are routed.
///
/// A closed list, and the reason `webhook_sources.target` can be data without being a hole: an
/// operator chooses which target a source feeds, and cannot invent one. A subject is a security
/// boundary (`ARCHITECTURE.md` S15 grants publish rights per subject), so the set of subjects this
/// process can reach is fixed at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Target {
    /// Submit an address for capture — the same work `POST /v2/captures` requests, reaching the
    /// same consumer through the same command type. A webhook is a second door into one room, not
    /// a second room.
    ContentCapture,
}

impl Target {
    /// Every target. The array length is the documented count, so adding a variant without
    /// extending this does not compile.
    pub const ALL: [Self; 1] = [Self::ContentCapture];

    /// The value stored in `platform_ingest.webhook_sources.target`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentCapture => "content.capture",
        }
    }

    /// The stored value, or `None` if this build serves no such target.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|target| target.as_str() == raw)
    }

    /// The `operations.operations.kind` a signal to this target creates.
    #[must_use]
    pub const fn operation_kind(self) -> &'static str {
        match self {
            Self::ContentCapture => "content.capture.submit",
        }
    }

    /// The contract type of the command published for it.
    #[must_use]
    pub const fn command_type(self) -> &'static str {
        match self {
            Self::ContentCapture => "content.capture.requested.v1",
        }
    }
}

impl core::fmt::Display for Target {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One served route: how it is reached, and how it is described. As in the client-facing crate,
/// the pair is what makes the `OpenAPI` document unable to drift from the router (ADR-0006).
struct Endpoint {
    doc: RouteDoc,
    handler: MethodRouter<Arc<IngestState>>,
}

/// Every route `ratatoskr-ingest` serves.
fn table() -> Vec<Endpoint> {
    vec![Endpoint {
        doc: webhook::DOC,
        handler: post(webhook::receive),
    }]
}

/// The public routes.
pub fn routes(state: IngestState) -> Router {
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

/// Whether the schema this crate reads has been applied.
///
/// `ratatoskr-ingest` deliberately does NOT run the migrations. `ARCHITECTURE.md` S18 gives it its
/// own least-privilege database role, and a role that may create a schema is not least-privilege;
/// the owner of the migrations is `ratatoskr-edge`, which applies them at startup. This check turns
/// "somebody started ingest against a database nobody migrated" into one sentence at startup rather
/// than into a Postgres error on the first inbound signal, hours later, in somebody else's log.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the catalogue itself cannot be read.
pub async fn schema_is_present(pool: &sqlx::PgPool) -> Result<bool, PersistenceError> {
    let present: Option<String> =
        sqlx::query_scalar("select to_regclass('platform_ingest.webhook_sources')::text")
            .fetch_one(pool)
            .await
            .map_err(PersistenceError::Query)?;
    Ok(present.is_some())
}

/// The payload types these routes carry.
fn register_schemas(generator: &mut schemars::SchemaGenerator) {
    generator.subschema_for::<webhook::WebhookSignal>();
    generator.subschema_for::<webhook::SignalAccepted>();
    generator.subschema_for::<ratatoskr_error_contracts::ErrorEnvelope>();
}
