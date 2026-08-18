//! The `ratatoskr-ingest` deployable.
//!
//! Milestone 1: typed configuration, telemetry and the operator listener. There is no public
//! listener and no work loop; configuring one is a startup failure until milestone 7 adds the first
//! inbound adapter (`ARCHITECTURE.md` S9).
//!
//! This file differs from `services/scheduler/src/main.rs` by the value of `ROLE` alone. That
//! duplication is deliberate: `AGENTS.md` requires the three deployables to stay separable as
//! binaries, so a `--role` flag that collapses them into one process is forbidden.

use std::process::ExitCode;

use platform_core::RuntimeRole;

const ROLE: RuntimeRole = RuntimeRole::Ingest;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("check-config") {
        return platform_http::check_config(ROLE);
    }
    platform_http::run(ROLE).await
}
