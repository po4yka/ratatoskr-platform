//! Redaction and cardinality — tests R-1 … R-6, plus R-2b for the `method` label.
//!
//! The log assertions run under a capturing subscriber installed for one thread. It is thirty
//! lines because `tracing-subscriber` is not a dependency of this crate, and because the property
//! under test is what the middleware *records*, not how a formatter renders it.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{PlatformConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState, admin_router, public_router};
use platform_telemetry::TelemetryGuard;
use ratatoskr_identifiers::EntityRef;
use serde_json::Value;
use tower::ServiceExt as _;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// A value that must never reach a log line.
const MARKER: &str = "zzmarkerzz";

/// The one telemetry installation this process gets.
fn telemetry() -> &'static TelemetryGuard {
    static GUARD: OnceLock<TelemetryGuard> = OnceLock::new();
    GUARD.get_or_init(|| {
        let config = PlatformConfig::defaults(RuntimeRole::Edge);
        platform_telemetry::init(&config.telemetry, RuntimeRole::Edge)
            .expect("telemetry must install exactly once in this process")
    })
}

/// The public listener of an edge process, with one templated route.
fn router() -> Router {
    telemetry();
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        request_timeout_seconds: 5,
        max_body_bytes: 1024,
    };
    let routes = Router::new().route("/probe/{id}", get(|| async { "ok" }));
    public_router(Arc::new(HttpState::new(RuntimeRole::Edge)), &config, routes)
}

async fn send(request: Request<Body>) -> (StatusCode, http::HeaderMap, String) {
    let response = router().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

/// The Prometheus text exposition of everything this process has recorded so far.
fn exposition() -> String {
    telemetry().metrics_handle().render()
}

/// Every value the label `name` takes in the exposition.
fn label_values(exposition: &str, name: &str) -> BTreeSet<String> {
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

/// R-1: the span records the route TEMPLATE; the identity in the path appears nowhere.
#[tokio::test]
async fn the_span_records_the_route_template_not_the_raw_path() {
    let log = Captured::default();
    let guard = tracing::subscriber::set_default(Capture::installing(&log));

    let (status, _, _) = send(
        Request::builder()
            .uri("/probe/018f-secret-value")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    drop(guard);

    assert_eq!(status, StatusCode::OK);
    let log = log.text();
    assert!(log.contains("http.route=/probe/{id}"), "{log}");
    assert!(!log.contains("secret-value"), "{log}");
    assert!(!log.contains("018f-"), "{log}");
}

/// R-2: the 404-scanning cardinality bomb is closed by construction.
#[tokio::test]
async fn a_thousand_unmatched_paths_produce_one_series() {
    for index in 0..1000_u32 {
        let uri = format!("/scan/{MARKER}-{index}");
        let (status, _, _) = send(Request::builder().uri(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    let exposition = exposition();

    assert!(
        !exposition.contains(MARKER),
        "a scanned path reached a label"
    );
    for label in label_values(&exposition, "route") {
        assert!(
            label == "<unmatched>" || label == "/probe/{id}",
            "unexpected route label {label}",
        );
    }
}

/// R-2b: the other half of the cardinality bomb — the method token.
///
/// A method is an attacker-chosen string off the unauthenticated public listener and hyper accepts
/// a 60 000-byte one, so a raw `method` label is a remote memory-exhaustion and scrape-cost attack
/// on the process and on the monitoring pipeline both, and it injects attacker text into the
/// operator plane. `ARCHITECTURE.md` S14; `THREAT_MODEL.md` "DoS/cost fan-out"; §5.4 "cardinality is
/// bounded by construction".
#[tokio::test]
async fn a_hundred_unknown_methods_produce_one_method_label() {
    let log = Captured::default();
    let guard = tracing::subscriber::set_default(Capture::installing(&log));

    for index in 0..100_u32 {
        let method = format!("{MARKER}{index}");
        let (status, _, _) = send(
            Request::builder()
                .method(method.as_str())
                .uri("/probe/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
    drop(guard);

    let exposition = exposition();

    assert!(
        !exposition.contains(MARKER),
        "a client-chosen method reached a label"
    );
    // The span field reads the same bounded value, so one rule bounds both surfaces.
    assert!(
        !log.text().contains(MARKER),
        "a client-chosen method reached a log line"
    );
    for label in label_values(&exposition, "method") {
        assert!(
            [
                "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
                "<other>"
            ]
            .contains(&label.as_str()),
            "unexpected method label {label}",
        );
    }
}

/// R-3: no header value and no query value reaches a log line.
#[tokio::test]
async fn no_header_or_query_value_reaches_a_log_line() {
    let log = Captured::default();
    let guard = tracing::subscriber::set_default(Capture::installing(&log));

    let (_, headers, body) = send(
        Request::builder()
            .uri(format!("/probe/1?token={MARKER}"))
            .header("authorization", format!("Bearer {MARKER}"))
            .header("cookie", format!("session={MARKER}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    drop(guard);

    let log = log.text();
    for surface in [&log, &body, &format!("{headers:?}"), &exposition()] {
        assert!(!surface.contains(MARKER), "the marker reached: {surface}");
        assert!(
            !surface.contains("Bearer"),
            "an authorization header reached: {surface}"
        );
    }
}

/// R-4: internal headers are not trusted from public ingress.
#[tokio::test]
async fn a_client_supplied_correlation_header_is_ignored() {
    let supplied = "correlation:01a01495-1f1d-7973-af61-680840cf4085";

    let (_, headers, body) = send(
        Request::builder()
            .uri("/nope")
            .header("x-correlation-id", supplied)
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    let served = headers.get("x-correlation-id").unwrap().to_str().unwrap();
    assert_ne!(served, supplied);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    assert_ne!(envelope["correlation_id"], supplied);
    assert_eq!(envelope["correlation_id"], served);
}

/// R-5: a refactor of the correlation to a `String` fails a test rather than a code review.
#[tokio::test]
async fn the_logged_correlation_is_a_parsable_contracts_entity_ref() {
    let log = Captured::default();
    let guard = tracing::subscriber::set_default(Capture::installing(&log));

    send(
        Request::builder()
            .uri("/probe/1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    drop(guard);

    let log = log.text();
    let logged = log
        .lines()
        .find_map(|line| line.strip_prefix("correlation_id="))
        .expect("the request span must record a correlation");
    let reference =
        EntityRef::parse(logged).expect("the correlation must be a contracts EntityRef");
    assert_eq!(reference.kind().as_str(), "correlation");
}

/// R-6: propagation works and a malformed header is not a denial-of-service surface.
#[tokio::test]
async fn an_inbound_traceparent_is_continued_and_a_malformed_one_does_not_fail_the_request() {
    let inbound = "4bf92f3577b34da6a3ce929d0e0e4736";
    let (status, _, body) = send(
        Request::builder()
            .uri("/nope")
            .header("traceparent", format!("00-{inbound}-00f067aa0ba902b7-01"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(envelope["trace_id"], inbound);

    let (status, _, body) = send(
        Request::builder()
            .uri("/nope")
            .header("traceparent", "not-a-trace-context")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    let fresh = envelope["trace_id"].as_str().unwrap();
    assert_eq!(fresh.len(), 32);
    assert_ne!(fresh, inbound);
}

/// T-5: the one middleware is attached to the public router only, so three probes at 1 Hz per
/// replica neither dominate the request rate nor pollute the latency histogram.
///
/// It lives here rather than in `crates/telemetry/tests/subscriber.rs` because it needs an axum
/// router, and here rather than in `tests/admin.rs` because the capturing subscriber that proves
/// "not spanned" already exists in this file.
#[tokio::test]
async fn admin_requests_are_neither_metered_nor_spanned() {
    telemetry();
    let state = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    state.mark_startup_complete();

    let log = Captured::default();
    let guard = tracing::subscriber::set_default(Capture::installing(&log));
    for _ in 0..20 {
        for route in ["/health/live", "/health/ready", "/metrics", "/version"] {
            let router = admin_router(Arc::clone(&state), String::new);
            let response = router
                .oneshot(Request::builder().uri(route).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{route}");
        }
    }
    drop(guard);

    let log = log.text();
    assert!(
        !log.contains("correlation_id="),
        "an admin request was spanned: {log}"
    );
    assert!(
        !log.contains("http.route="),
        "an admin request was spanned: {log}"
    );
    for label in label_values(&exposition(), "route") {
        assert!(
            !label.starts_with("/health"),
            "an admin request was metered as {label}"
        );
        assert!(
            label != "/metrics" && label != "/version",
            "an admin request was metered"
        );
    }
}

/// Everything a subscriber was asked to record, one `field=value` per line.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl Captured {
    fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// A subscriber that records field values and nothing else.
struct Capture {
    sink: Captured,
    next: AtomicU64,
}

impl Capture {
    fn installing(sink: &Captured) -> Self {
        Self {
            sink: sink.clone(),
            next: AtomicU64::new(1),
        }
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        span.record(&mut Writer(&self.sink));
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &Id, values: &Record<'_>) {
        values.record(&mut Writer(&self.sink));
    }

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        event.record(&mut Writer(&self.sink));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Writes `field=value` lines into a [`Captured`].
struct Writer<'a>(&'a Captured);

impl Visit for Writer<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.write(field, value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.write(field, &format!("{value:?}"));
    }
}

impl Writer<'_> {
    fn write(&self, field: &Field, value: &str) {
        let mut sink = self.0.0.lock().unwrap();
        sink.push_str(field.name());
        sink.push('=');
        sink.push_str(value);
        sink.push('\n');
    }
}
