//! T-3: §6.7 step 6 — flushing the span exporter must be safe to reach twice.
//!
//! Its own test binary, and therefore its own process: shutting the tracer provider down makes
//! every later span context invalid, which is exactly what `tests/subscriber.rs` T-1 asserts is
//! not the case.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use opentelemetry_sdk::trace::SdkTracerProvider;
use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_telemetry::TelemetryError;

/// T-3 — a second SIGTERM during the shutdown sequence reaches step 6 again.
///
/// `TelemetryGuard::shutdown(self)` consumes the guard, so "idempotent" cannot mean "call it
/// twice" — the second call does not compile. What the claim rests on is the provider underneath,
/// whose second shutdown returns `AlreadyShutdown` instead of panicking; that is the branch
/// `TelemetryGuard::shutdown` logs a warning for, and it is asserted directly below.
#[test]
fn telemetry_shutdown_is_idempotent_and_does_not_panic() {
    let config = PlatformConfig::defaults(RuntimeRole::Scheduler).telemetry;
    let guard =
        platform_telemetry::init(&config, RuntimeRole::Scheduler).expect("telemetry must install");

    guard.shutdown();

    // The guarded property, on the same type the guard owns: a repeated shutdown is an error to
    // report, never a panic on the path a pod takes when an operator signals twice.
    let provider = SdkTracerProvider::builder().build();
    provider.shutdown().expect("the first shutdown succeeds");
    let again = provider.shutdown();
    assert!(
        again.is_err(),
        "a second shutdown must report rather than succeed silently: {again:?}"
    );

    // The provider is gone with the guard, so a second shutdown can only arrive as a second `init`,
    // which the already-installed globals reject instead of leaving a half-built stack behind.
    let again = platform_telemetry::init(&config, RuntimeRole::Scheduler);
    assert!(
        matches!(again, Err(TelemetryError::AlreadyInstalled)),
        "a second install must be refused, not duplicated",
    );

    // The process is still healthy: logging after the flush neither panics nor deadlocks.
    tracing::info!("platform.shutdown completed");
}
