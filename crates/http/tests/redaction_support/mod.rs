//! Helpers shared by the redaction test binaries: the one telemetry installation of the process,
//! the public router with one templated route, and the Prometheus exposition.

#![allow(
    dead_code,
    reason = "each redaction test binary uses a part of these helpers"
)]
#![allow(
    clippy::string_slice,
    reason = "ASCII fixtures parsed by offset in a test binary"
)]

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{PlatformConfig, PublicConfig};
use platform_http::{HttpState, public_router};
use platform_telemetry::TelemetryGuard;
use tower::ServiceExt as _;

/// A value that must never reach a log line.
pub(crate) const MARKER: &str = "zzmarkerzz";

/// The one telemetry installation this process gets.
pub(crate) fn telemetry() -> &'static TelemetryGuard {
    static GUARD: OnceLock<TelemetryGuard> = OnceLock::new();
    GUARD.get_or_init(|| {
        let config = PlatformConfig::defaults(RuntimeRole::Edge);
        platform_telemetry::init(&config.telemetry, RuntimeRole::Edge)
            .expect("telemetry must install exactly once in this process")
    })
}

/// The public listener of an edge process, with one templated route.
pub(crate) fn router() -> Router {
    telemetry();
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        request_timeout_seconds: 5,
        max_body_bytes: 1024,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    let routes = Router::new().route("/probe/{id}", get(|| async { "ok" }));
    public_router(Arc::new(HttpState::new(RuntimeRole::Edge)), &config, routes)
}

pub(crate) async fn send(request: Request<Body>) -> (StatusCode, http::HeaderMap, String) {
    let response = router().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

/// The Prometheus text exposition of everything this process has recorded so far.
pub(crate) fn exposition() -> String {
    telemetry().metrics_handle().render()
}

/// Every value the label `name` takes in the exposition.
pub(crate) fn label_values(exposition: &str, name: &str) -> BTreeSet<String> {
    let prefix = format!("{name}=\"");
    let mut labels = BTreeSet::new();
    for start in exposition.match_indices(prefix.as_str()) {
        let rest = &exposition[start.0 + prefix.len()..];
        if let Some(end) = rest.find('"') {
            labels.insert(rest[..end].to_owned());
        }
    }
    labels
}
