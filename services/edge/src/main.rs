//! The `ratatoskr-edge` deployable.
//!
//! Milestone 1: typed configuration, telemetry, the operator listener, and a public listener whose
//! only behaviour is a contract `ErrorEnvelope` on every non-2xx. The public API surface arrives
//! with milestone 5.

use std::process::ExitCode;

use platform_core::RuntimeRole;

const ROLE: RuntimeRole = RuntimeRole::Edge;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("check-config") {
        return platform_http::check_config(ROLE);
    }
    platform_http::run(ROLE).await
}
