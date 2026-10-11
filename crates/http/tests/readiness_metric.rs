//! The readiness gauge follows `is_ready` (XR-021 CONTRACTS.md R3-13).
//!
//! In a binary of its own because the Prometheus recorder and the `platform_readiness{role}` series
//! are process-wide: in `admin.rs` any test that builds an Edge state moves the same series, so an
//! assertion on its value there would depend on which tests happen to run at the same moment.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_http::RuntimeState;

/// A down archive receiver is reported but does not take Edge out of rotation, so the gauge that
/// routing and alerting read stays at 1.
#[test]
fn the_readiness_gauge_ignores_a_down_archive_receiver() {
    let config = PlatformConfig::defaults(RuntimeRole::Edge);
    // Installed before any state exists: the gauge is set when a state is built and when it changes,
    // and a value set before the recorder is installed is lost.
    let guard = platform_telemetry::init(&config.telemetry, RuntimeRole::Edge)
        .expect("telemetry installs once in this process");

    let state = RuntimeState::new(RuntimeRole::Edge);
    state.mark_startup_complete();
    state.set_database_reachable(true);
    state.set_bus_reachable(true);
    state.set_archive_staging_ready(true);
    for provider in ["chatgpt", "claude"] {
        state.set_archive_receipt_ready(provider, false);
        state.set_archive_report_ready(provider, false);
    }
    assert!(state.is_ready());

    let rendered = guard.metrics_handle().render();
    assert!(
        rendered
            .lines()
            .any(|line| line == "platform_readiness{role=\"edge\"} 1"),
        "platform_readiness follows is_ready: {rendered}"
    );

    state.set_archive_staging_ready(false);
    let rendered = guard.metrics_handle().render();
    assert!(
        rendered
            .lines()
            .any(|line| line == "platform_readiness{role=\"edge\"} 0"),
        "an unwritable staging root still takes the gauge to 0: {rendered}"
    );
}
