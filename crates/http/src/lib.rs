//! The shared HTTP harness every deployable runs: `run(role)` and nothing else in a `main`.
//!
//! - [`run`] and [`check_config`] — the whole process lifecycle, so the three binaries cannot
//!   drift.
//! - [`public_router`] — THE public-router middleware and the layer stack under it.
//! - [`admin_router`] — liveness, readiness, metrics and version, on the operator listener only.
//! - [`RuntimeState`] — the two facts readiness is computed from.
//! - [`serve`] and [`drain_and_close`] — the drain-then-close-then-flush sequence.
//!
//! # The one documented exception
//!
//! Every non-2xx response from the **public** listener carries a contract `ErrorEnvelope`, built in
//! exactly one place, `fault::render`. The **admin** listener carries none: `/health/ready`
//! returning 503 must tell an operator WHICH check failed, and `ErrorEnvelope` has no member for
//! that.
//!
//! # Exit codes
//!
//! | Code | Meaning |
//! |---|---|
//! | `0` | Clean start and clean shutdown |
//! | `1` | Runtime startup failure: telemetry initialisation, or a listener that could not bind |
//! | `78` | `EX_CONFIG` — the configuration is unreadable or invalid; nothing was bound |

mod admin;
pub mod fault;
mod lifecycle;
pub mod limit;
pub mod observe;
mod shutdown;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_persistence::Database;
use platform_telemetry::identity;
use tokio::net::TcpListener;
use tracing::field::Empty;

pub use crate::admin::admin_router;
pub use crate::fault::{AuthoredFailure, preserve_contract_error, reject};
pub use crate::lifecycle::{Check, CheckName, CheckReason, CheckState, HttpState, RuntimeState};
pub use crate::limit::{ActorLimiter, Concurrency};
pub use crate::observe::{RequestContext, public_router};
pub use crate::shutdown::{Served, ShutdownOutcome, drain_and_close, serve};

/// What a binary contributes to its public listener.
///
/// A trait rather than a `Router` argument, because the routes need the configuration this function
/// loads — the database URL above all — and a binary cannot build them before `run` has read it.
/// A trait rather than a closure, because the future must borrow the configuration and an
/// `async` closure that does so is not expressible without naming the lifetime anyway.
///
/// `ratatoskr-ingest` and `ratatoskr-scheduler` use [`NoPublicRoutes`]: they bind no public listener
/// at all, so their contribution is not "an empty router", it is "there is no router".
pub trait PublicRoutes {
    /// Build the routes, or explain why the process must not start.
    ///
    /// Returning an error here is a startup failure, not a request failure: a binary that cannot
    /// reach the database it needs must refuse to report itself ready rather than serve 500s.
    ///
    /// `health` is the same [`RuntimeState`] the readiness probe reads. It is passed rather than
    /// rebuilt because `GET /v1/capabilities` reports whether a dependency is healthy (ADR-0008),
    /// and a second source for that fact could disagree with the first.
    fn build(
        self,
        config: &PlatformConfig,
        health: &Arc<RuntimeState>,
    ) -> impl Future<Output = Result<Serving, String>> + Send;
}

/// What a binary serves, and what it runs alongside.
#[derive(Debug, Default)]
pub struct Serving {
    /// The public routes.
    pub routes: Router,
    /// The pool, when this role has one. `run` probes it for readiness and closes it after the
    /// grace window, so a binary does not have to remember to.
    pub database: Option<Database>,
    /// Background work that must stop when the process does — the outbox publisher, the event
    /// consumer. Aborted after the listeners close, never before: a task that is still publishing
    /// when the listener stops is finishing work a request already committed to.
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// The contribution of a role that serves no public API.
#[derive(Debug, Clone, Copy)]
pub struct NoPublicRoutes;

impl PublicRoutes for NoPublicRoutes {
    async fn build(
        self,
        _config: &PlatformConfig,
        _health: &Arc<RuntimeState>,
    ) -> Result<Serving, String> {
        Ok(Serving::default())
    }
}

/// How often the database prober asks whether the dependency is still there.
///
/// Five seconds: long enough that the probe is not itself load, short enough that a readiness state
/// is never more than one scrape interval stale — the metrics stack on the deployment target scrapes
/// every fifteen.
const DATABASE_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// The whole process lifecycle for one runtime role. Each binary's `main` is this call and nothing
/// else.
///
/// Sequence, in this order and no other:
///
/// 1. `platform_core::config::load(role)` — on failure write `ConfigError::report` to stderr and
///    exit `78`.
/// 2. `platform_telemetry::init` — on failure write to stderr, exit `1`. Telemetry is initialised
///    AFTER validation so an invalid `log_filter` is a configuration error, not a failure inside
///    subscriber setup where nothing can report it.
/// 3. Open `platform.startup`. Log the effective configuration at INFO (safe by type) and the
///    non-fatal warnings. `platform_build_info` is set by `platform_telemetry::init`.
/// 4. Bind the admin listener; bind the public listener when `config.public` is `Some`. On failure
///    log at ERROR and exit `1`.
/// 5. [`RuntimeState::mark_startup_complete`] — readiness flips to 200.
/// 6. Serve both listeners until SIGTERM or SIGINT, then [`drain_and_close`].
/// 7. `TelemetryGuard::shutdown()`; exit `0`.
pub async fn run<R: PublicRoutes>(role: RuntimeRole, routes: R) -> ExitCode {
    let config = match platform_core::config::load(role) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{}", error.report(role));
            return ExitCode::from(error.exit_code());
        }
    };

    let guard = match platform_telemetry::init(&config.telemetry, role) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!(
                "{}: refusing to start; telemetry could not be initialised: {error}",
                role.binary_name()
            );
            return ExitCode::FAILURE;
        }
    };

    let started = Instant::now();
    let startup = tracing::info_span!(
        "platform.startup",
        role = role.as_str(),
        version = identity::VERSION,
        git_sha = identity::GIT_SHA,
        duration_ms = Empty,
    );
    startup.in_scope(|| announce(role, &config));

    let runtime = Arc::new(RuntimeState::new(role));
    let http = Arc::new(HttpState::new(role));
    let metrics = guard.metrics_handle();

    let Serving {
        routes: public_routes,
        database,
        tasks,
    } = match routes.build(&config, &runtime).await {
        Ok(built) => built,
        Err(reason) => {
            startup.in_scope(|| {
                tracing::error!(%reason, "the public routes could not be built");
            });
            return ExitCode::FAILURE;
        }
    };

    let prober = start_database_prober(database.as_ref(), &runtime).await;

    let Some(servers) = startup
        .in_scope(|| bind_listeners(&config, &runtime, &http, metrics, public_routes))
        .await
    else {
        return ExitCode::FAILURE;
    };

    runtime.mark_startup_complete();
    startup.record("duration_ms", observe::duration_ms(started.elapsed()));
    startup.in_scope(|| {
        tracing::info!(
            admin = %config.admin.bind,
            public = config.public.as_ref().map(|public| public.bind.to_string()),
            "startup complete",
        );
    });
    drop(startup);

    shutdown::signal().await;
    drain_and_close(
        &runtime,
        &config.shutdown,
        servers,
        http.in_flight(),
        shutdown::signal(),
    )
    .await;

    if let Some(prober) = prober {
        prober.abort();
    }
    for task in tasks {
        task.abort();
    }
    if let Some(database) = database {
        // After the listener stopped accepting and the grace window closed, so an in-flight request
        // kept its connection for its whole life.
        database.close().await;
    }

    guard.shutdown();
    ExitCode::SUCCESS
}

/// `<binary> check-config`: load and validate without binding anything; write the effective
/// configuration or the failure report; exit `0` or `78`.
///
/// It exists so a `ConfigMap` can be validated in CI or an init container before a pod is allowed
/// to start. Both outputs go to stderr: no subscriber exists yet, and the workspace forbids writing
/// to stdout so that a stray line can never be mistaken for a log record.
#[must_use]
pub fn check_config(role: RuntimeRole) -> ExitCode {
    match platform_core::config::load(role) {
        Ok(config) => {
            // Safe by type: the one secret member renders as `[REDACTED]` however deeply nested.
            eprintln!(
                "{}: configuration is valid.\n{config:#?}",
                role.binary_name()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}", error.report(role));
            ExitCode::from(error.exit_code())
        }
    }
}

/// The single INFO line that says what the process actually believes, and the two non-fatal
/// warnings. Safe by type: `SecretString` has no `Display` and renders as `[REDACTED]`.
fn announce(role: RuntimeRole, config: &PlatformConfig) {
    tracing::info!(
        config = ?config,
        role = %role,
        version = identity::VERSION,
        git_sha = identity::GIT_SHA,
        "effective configuration",
    );
    if !config.admin.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %config.admin.bind,
            "the admin plane is not bound to a loopback address; it must never be published \
             through an ingress",
        );
    }
    if config.telemetry.otlp.is_none() {
        tracing::warn!(
            "no OTLP endpoint is configured; spans are created and carry real trace ids, but \
             nothing is exported",
        );
    }
}

/// Probe the database once, then keep probing until the process stops.
///
/// Split out of [`run`] to keep it inside the workspace's function-length lint, along the boundary
/// that means something: this is the only thing in the process that decides whether the dependency
/// is reachable.
///
/// The first probe happens BEFORE the listener opens, so a process never reports itself ready with
/// an unverified dependency.
async fn start_database_prober(
    database: Option<&Database>,
    runtime: &Arc<RuntimeState>,
) -> Option<tokio::task::JoinHandle<()>> {
    let database = database?;
    runtime.set_database_reachable(database.ping().await.is_ok());

    let database = database.clone();
    let runtime = Arc::clone(runtime);
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(DATABASE_PROBE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            runtime.set_database_reachable(database.ping().await.is_ok());
        }
    }))
}

/// Bind the admin listener, and the public one when the role has it.
///
/// Extracted from [`run`] to keep it inside the workspace's function-length lint, along a boundary
/// that means something: everything before it prepares the process, this opens its sockets, and
/// everything after it serves.
///
/// `None` on failure; the caller exits `1`. The error is logged here, inside the startup span, so it
/// carries the same fields as every other startup record.
async fn bind_listeners(
    config: &PlatformConfig,
    runtime: &Arc<RuntimeState>,
    http: &Arc<HttpState>,
    metrics: metrics_exporter_prometheus::PrometheusHandle,
    public_routes: Router,
) -> Option<Vec<crate::shutdown::Served>> {
    let admin = match TcpListener::bind(config.admin.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(bind = %config.admin.bind, %error, "the admin listener could not bind");
            return None;
        }
    };
    let mut servers = vec![serve(
        admin,
        admin_router(Arc::clone(runtime), move || metrics.render()),
    )];

    if let Some(public) = config.public.as_ref() {
        match TcpListener::bind(public.bind).await {
            Ok(listener) => servers.push(serve(
                listener,
                public_router(Arc::clone(http), public, public_routes),
            )),
            Err(error) => {
                tracing::error!(bind = %public.bind, %error, "the public listener could not bind");
                return None;
            }
        }
    }

    Some(servers)
}
