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
//! returning 503 must tell the orchestrator WHICH check failed, and `ErrorEnvelope` has no member
//! for that.
//!
//! # Exit codes
//!
//! | Code | Meaning |
//! |---|---|
//! | `0` | Clean start and clean shutdown |
//! | `1` | Runtime startup failure: telemetry initialisation, or a listener that could not bind |
//! | `78` | `EX_CONFIG` — the configuration is unreadable or invalid; nothing was bound |

mod admin;
mod fault;
mod lifecycle;
mod observe;
mod shutdown;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_telemetry::identity;
use tokio::net::TcpListener;
use tracing::field::Empty;

pub use crate::admin::admin_router;
pub use crate::lifecycle::{Check, CheckName, CheckReason, CheckState, HttpState, RuntimeState};
pub use crate::observe::{RequestContext, public_router};
pub use crate::shutdown::{Served, ShutdownOutcome, drain_and_close, serve};

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
pub async fn run(role: RuntimeRole) -> ExitCode {
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

    let admin = match TcpListener::bind(config.admin.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            startup.in_scope(|| {
                tracing::error!(bind = %config.admin.bind, %error, "the admin listener could not bind");
            });
            return ExitCode::FAILURE;
        }
    };
    let mut servers = vec![serve(
        admin,
        admin_router(Arc::clone(&runtime), move || metrics.render()),
    )];

    if let Some(public) = config.public.as_ref() {
        match TcpListener::bind(public.bind).await {
            Ok(listener) => servers.push(serve(
                listener,
                public_router(Arc::clone(&http), public, Router::new()),
            )),
            Err(error) => {
                startup.in_scope(|| {
                    tracing::error!(bind = %public.bind, %error, "the public listener could not bind");
                });
                return ExitCode::FAILURE;
            }
        }
    }

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
