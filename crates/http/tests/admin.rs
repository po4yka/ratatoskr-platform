//! Health, readiness, metrics and version — tests H-1 … H-12.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_http::{HttpState, RuntimeState, admin_router, public_router};
use platform_telemetry::TelemetryGuard;
use serde_json::Value;
use tower::ServiceExt as _;

/// The one telemetry installation this process gets: a global subscriber and a global metrics
/// recorder can each be installed exactly once.
fn telemetry() -> &'static TelemetryGuard {
    static GUARD: OnceLock<TelemetryGuard> = OnceLock::new();
    GUARD.get_or_init(|| {
        let config = PlatformConfig::defaults(RuntimeRole::Edge);
        platform_telemetry::init(&config.telemetry, RuntimeRole::Edge)
            .expect("telemetry must install exactly once in this process")
    })
}

/// The admin router of a process in `state`, wired to the real Prometheus renderer.
fn admin(state: &Arc<RuntimeState>) -> Router {
    let guard = telemetry();
    admin_router(Arc::clone(state), move || guard.metrics_handle().render())
}

/// A process that has bound every listener.
fn started(role: RuntimeRole) -> Arc<RuntimeState> {
    let state = Arc::new(RuntimeState::new(role));
    state.mark_startup_complete();
    state
}

async fn get(router: Router, uri: &str) -> (StatusCode, HeaderMap, String) {
    let response = router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

async fn json(router: Router, uri: &str) -> (StatusCode, Value) {
    let (status, _, body) = get(router, uri).await;
    (status, serde_json::from_str(&body).unwrap())
}

/// H-1: liveness never consults anything, so a process whose every readiness check fails is still
/// alive. The anti-restart-storm guarantee.
#[tokio::test]
async fn liveness_is_200_while_every_readiness_check_fails() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    state.begin_draining();

    let (status, body) = json(admin(&state), "/health/live").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "live");

    let (status, body) = json(admin(&state), "/health/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    for check in body["checks"].as_array().unwrap() {
        assert_eq!(check["state"], "fail", "{check}");
    }
}

/// H-2: nothing routes traffic to a half-initialised process.
#[tokio::test]
async fn readiness_is_503_before_startup_completes() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Ingest));

    let (status, body) = json(admin(&state), "/health/ready").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["state"], "not_ready");
    assert_eq!(body["checks"][1]["name"], "startup");
    assert_eq!(body["checks"][1]["reason"], "startup_incomplete");
}

/// H-3: the drain gate. Readiness fails the instant a signal arrives while the listener still
/// answers — the anti-502 guarantee.
#[tokio::test]
async fn readiness_is_503_while_liveness_and_the_listener_still_answer_200() {
    let state = started(RuntimeRole::Edge);
    let (ready, _) = json(admin(&state), "/health/ready").await;
    assert_eq!(ready, StatusCode::OK);

    state.begin_draining();

    let (ready, body) = json(admin(&state), "/health/ready").await;
    assert_eq!(ready, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["checks"][0]["reason"], "shutdown_requested");

    let (live, body) = json(admin(&state), "/health/live").await;
    assert_eq!(
        live,
        StatusCode::OK,
        "liveness must answer throughout the drain"
    );
    assert_eq!(body["state"], "live");
}

/// H-4: no hostname, port, DSN or driver message reaches a probe body.
#[tokio::test]
async fn readiness_reports_the_named_checks_and_nothing_else() {
    let state = started(RuntimeRole::Edge);

    let (_, body) = json(admin(&state), "/health/ready").await;

    // `serde_json` parses into a sorted map, so this is the member SET; the wire order is what
    // H-6 pins.
    let members: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(members, ["checks", "role", "state"]);
    let checks = body["checks"].as_array().unwrap();
    let names: Vec<&Value> = checks.iter().map(|check| &check["name"]).collect();
    assert_eq!(names, ["drain", "startup"]);
    for check in checks {
        for member in check.as_object().unwrap().keys() {
            assert!(
                ["name", "state", "reason"].contains(&member.as_str()),
                "unexpected member {member}",
            );
        }
    }
}

#[tokio::test]
async fn archive_readiness_reports_each_provider_path_independently() {
    let state = started(RuntimeRole::Edge);
    state.set_archive_staging_ready(true);
    state.set_archive_receipt_ready("chatgpt", true);
    state.set_archive_report_ready("chatgpt", false);
    state.set_archive_receipt_ready("claude", false);
    state.set_archive_report_ready("claude", true);

    let (status, body) = json(admin(&state), "/health/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let checks = body["checks"].as_array().expect("readiness checks");
    for (name, expected) in [
        ("archive_chatgpt_receipt", "pass"),
        ("archive_chatgpt_report", "fail"),
        ("archive_claude_receipt", "fail"),
        ("archive_claude_report", "pass"),
        ("archive_staging", "pass"),
    ] {
        assert_eq!(
            checks
                .iter()
                .find(|check| check["name"] == name)
                .expect("the named archive check")["state"],
            expected
        );
    }
}

/// H-5: a reason is a token from a closed set, never a formatted dependency error.
#[tokio::test]
async fn the_readiness_reason_comes_from_the_closed_set() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Scheduler));
    state.begin_draining();

    let (_, body) = json(admin(&state), "/health/ready").await;

    for check in body["checks"].as_array().unwrap() {
        let reason = check["reason"].as_str().unwrap();
        assert!(
            ["startup_incomplete", "shutdown_requested"].contains(&reason),
            "unexpected reason {reason}",
        );
    }
}

/// H-6: deterministic ordering, so `diff` is a usable tool at 03:00.
#[tokio::test]
async fn the_readiness_body_is_byte_identical_across_consecutive_calls() {
    let state = started(RuntimeRole::Edge);

    let (_, _, first) = get(admin(&state), "/health/ready").await;
    let (_, _, second) = get(admin(&state), "/health/ready").await;

    assert_eq!(first, second);
}

/// H-7: a cached "ready" is a routing decision made from stale data.
#[tokio::test]
async fn admin_responses_are_no_store() {
    let state = started(RuntimeRole::Edge);

    for route in ["/health/live", "/health/ready", "/metrics", "/version"] {
        let (_, headers, _) = get(admin(&state), route).await;
        assert_eq!(
            headers.get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "{route}",
        );
    }
}

/// H-8: one axum route calling `handle.render()`; no second HTTP server.
#[tokio::test]
async fn metrics_renders_prometheus_text_exposition() {
    let state = started(RuntimeRole::Edge);

    let (status, headers, body) = get(admin(&state), "/metrics").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/plain; version=0.0.4",
    );
    assert!(body.contains("platform_build_info"), "{body}");
    assert!(body.contains("# TYPE"), "{body}");
}

/// H-9: the build fingerprint lives on the admin plane, not the public one.
#[tokio::test]
async fn version_reports_service_role_version_and_git_sha() {
    let state = started(RuntimeRole::Edge);

    let (status, body) = json(admin(&state), "/version").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["service"], "ratatoskr-platform");
    assert_eq!(body["role"], "edge");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["git_sha"].is_string());
    assert!(body["rust_version"].is_string());
}

/// H-10: the probes are not on the public listener, and the public 404 carries an `ErrorEnvelope`.
#[tokio::test]
async fn probes_are_not_served_on_the_public_listener() {
    let config = PlatformConfig::defaults(RuntimeRole::Edge);
    let public = config.public.as_ref().unwrap();
    let router = public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        public,
        Router::new(),
    );

    let (status, body) = json(router, "/health/live").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "platform.route.not_found");
}

/// H-11: the scheduler's health surface is not a second implementation.
#[tokio::test]
async fn the_admin_router_is_identical_for_every_role() {
    for role in RuntimeRole::ALL {
        let state = started(role);
        for route in ["/health/live", "/health/ready", "/metrics", "/version"] {
            let (status, _, _) = get(admin(&state), route).await;
            assert_eq!(status, StatusCode::OK, "{role} {route}");
        }
        let (_, body) = json(admin(&state), "/health/ready").await;
        assert_eq!(body["role"], role.as_str());
    }
}

/// H-12: `ARCHITECTURE.md` S18 — the scheduler binds exactly one listener and it serves only
/// probes. `run` builds a public router only when `config.public` is `Some`, and the defaults and
/// validation rule V1 make that impossible for the scheduler.
#[test]
fn the_scheduler_builds_no_public_router() {
    assert!(!RuntimeRole::Scheduler.may_have_public_listener());
    // There is no `PublicConfig` to build a public router from, which is the claim, and it is a
    // property of the defaults rather than of this process's environment. The `config::load` call
    // that used to stand here read the ambient environment unjailed, so an exported
    // `RATATOSKR__PUBLIC__BIND` — the variable the neighbouring tests set — turned it red for an
    // unrelated reason; C-1 and C-8 cover the load, under a `figment::Jail`.
    assert!(
        PlatformConfig::defaults(RuntimeRole::Scheduler)
            .public
            .is_none()
    );
}
