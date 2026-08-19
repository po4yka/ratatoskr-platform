//! The `ratatoskr-scheduler` deployable.
//!
//! Milestone 1: typed configuration, telemetry and the operator listener. It waits for the shutdown
//! signal and serves no requests. A public listener is refused permanently, not only at this
//! milestone (`ARCHITECTURE.md` S18).
//!
//! This file differs from `services/ingest/src/main.rs` by the value of `ROLE` alone. That
//! duplication is deliberate: `AGENTS.md` requires the three deployables to stay separable as
//! binaries, so a `--role` flag that collapses them into one process is forbidden.

use std::process::ExitCode;

use platform_core::RuntimeRole;

const ROLE: RuntimeRole = RuntimeRole::Scheduler;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("check-config") {
        return platform_http::check_config(ROLE);
    }
    platform_http::run(ROLE, platform_http::NoPublicRoutes).await
}
