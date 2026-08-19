//! The `ratatoskr-scheduler` deployable.
//!
//! Milestone 1: typed configuration, telemetry and the operator listener. It waits for the shutdown
//! signal and serves no requests. A public listener is refused permanently, not only at this
//! milestone (`ARCHITECTURE.md` S18).
//!
//! It no longer resembles `services/ingest/src/main.rs`: milestone 7 gave ingest a listener and a
//! database, and this binary has neither. Open question Q10 asked for exactly that signal before
//! milestone 7, and the answer is that the two have diverged and must not be collapsed.

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
