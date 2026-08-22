//! T-1, T-2, T-4 and T-6: the subscriber, the minted correlation, and the exposed instrument set.
//!
//! T-3 lives in `tests/shutdown.rs` because it destroys the tracer provider T-1 needs alive, and
//! every `#[test]` in one file shares one process. T-5 (`admin requests are neither metered nor
//! spanned`) needs an axum router, which this crate deliberately does not depend on; it belongs to
//! `crates/http/tests`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::OnceLock;

use metrics_exporter_prometheus::PrometheusHandle;
use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_telemetry::{TelemetryGuard, correlation, identity, metrics};
use ratatoskr_identifiers::EntityRef;

/// The one telemetry installation this process gets: a global subscriber and a global metrics
/// recorder can each be installed exactly once.
fn installed() -> &'static TelemetryGuard {
    static GUARD: OnceLock<TelemetryGuard> = OnceLock::new();
    GUARD.get_or_init(|| {
        let config = PlatformConfig::defaults(RuntimeRole::Edge).telemetry;
        assert!(
            config.otlp.is_none(),
            "the default configuration must not export spans"
        );
        platform_telemetry::init(&config, RuntimeRole::Edge).expect("telemetry must install")
    })
}

/// The Prometheus text-exposition renderer of the installed recorder.
fn exposition() -> PrometheusHandle {
    installed().metrics_handle()
}

/// T-1 — the default developer experience needs no infrastructure, and `trace_id` is genuine.
#[test]
fn init_succeeds_and_yields_a_valid_trace_id_with_no_otlp_endpoint_and_no_collector() {
    let _installed = installed();

    let span = tracing::info_span!("platform.startup", role = "edge");
    let trace_id = correlation::trace_id_of(&span)
        .expect("an exporterless provider still mints a valid W3C trace id");

    assert_eq!(trace_id.as_str().len(), 32);
    assert!(
        trace_id
            .as_str()
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "a W3C trace id is 32 lowercase hex characters, got {trace_id}",
    );
    assert_ne!(
        trace_id.as_str(),
        "0".repeat(32),
        "an all-zero id means the context was invalid"
    );
}

/// T-2 — ADR-0007: the correlation is a contracts `EntityRef`, never a `String`.
#[test]
fn the_minted_correlation_is_a_parsable_entity_ref_of_kind_correlation() {
    let minted = correlation::mint_correlation();
    let rendered = minted.to_string();

    assert!(rendered.starts_with("correlation:"), "got {rendered}");
    assert_eq!(minted.kind().as_str(), "correlation");

    let reparsed = EntityRef::parse(&rendered).expect("the minted value must parse back");
    assert_eq!(reparsed, minted);

    assert_ne!(
        correlation::mint_correlation(),
        minted,
        "each unit of work gets its own correlation",
    );
}

/// T-4 — a rename silently breaks every dashboard and every alert, so it must break a test first.
#[test]
fn the_metric_name_set_is_exactly_the_documented_set() {
    assert_eq!(
        metrics::ALL,
        [
            "http_server_request_duration_seconds",
            "platform_readiness",
            "platform_build_info",
            "platform_scheduler_drift_seconds",
            "platform_scheduler_occurrences_total",
            "platform_capability_available",
            "platform_auth_decisions_total",
            "platform_rate_limit_decisions_total",
            "platform_operation_transitions_total",
            "platform_operations",
            "platform_operations_oldest_unterminated_age_seconds",
            "platform_operations_reconciled_total",
            "platform_outbox_pending",
            "platform_outbox_dead_lettered",
            "platform_outbox_oldest_pending_age_seconds",
            "platform_inbox_unprocessed",
            "platform_outbox_publications_total",
            "platform_idempotency_outcomes_total",
            "platform_sse_connections",
            "platform_sse_delivery_lag_seconds"
        ],
    );

    for line in exposition().render().lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let name = rest
                .split_whitespace()
                .next()
                .expect("a TYPE line names a metric");
            assert!(
                metrics::ALL.contains(&name),
                "the recorder exported `{name}`, which is not in the documented set",
            );
        }
    }
}

/// T-6 — what is actually running is the first thing anyone looks at.
#[test]
fn build_info_is_exported_with_version_git_sha_and_rust_version() {
    let rendered = exposition().render();

    let line = rendered
        .lines()
        .find(|line| line.starts_with(metrics::PLATFORM_BUILD_INFO) && !line.starts_with('#'))
        .expect("platform_build_info must be exported");

    for label in [
        format!("role=\"{}\"", RuntimeRole::Edge.as_str()),
        format!("version=\"{}\"", identity::VERSION),
        format!("git_sha=\"{}\"", identity::GIT_SHA),
        format!("rust_version=\"{}\"", identity::RUST_VERSION),
    ] {
        assert!(line.contains(&label), "`{label}` missing from `{line}`");
    }
    assert!(line.ends_with(" 1"), "build info is always 1, got `{line}`");
}
