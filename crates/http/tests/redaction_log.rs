//! Redaction tests that capture the log — R-1, R-2b, R-3, R-5 and T-5.
//!
//! The log assertions run under a capturing subscriber installed for one thread
//! (`tracing::subscriber::set_default`). It is thirty lines because `tracing-subscriber` is not a
//! dependency of this crate, and because the property under test is what the middleware *records*,
//! not how a formatter renders it.
//!
//! These tests are apart from the others, and they run one at a time, on purpose. While a scoped
//! subscriber is the only registered dispatcher, `tracing-core` computes the interest of a callsite
//! that another thread registers first from the default dispatcher of THAT thread, and caches
//! `never` for every thread when it has none. A test without a capture that sends the first request
//! through the public router on another thread at that time would disable the request span here,
//! and the capture would prove nothing. Every test of this binary takes [`SERIAL`] before it does
//! anything else and installs its capture before its first request, so each callsite of a request
//! is registered on a thread whose default dispatcher is a capture. A test that does not capture
//! the log belongs in `redaction.rs`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod redaction_support;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use http::{Request, StatusCode};
use platform_core::RuntimeRole;
use platform_http::{RuntimeState, admin_router};
use ratatoskr_identifiers::EntityRef;
use redaction_support::{MARKER, exposition, label_values, send, telemetry};
use tower::ServiceExt as _;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// One test of this binary at a time; see the module documentation.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// R-1: the span records the route TEMPLATE; the identity in the path appears nowhere.
#[tokio::test]
async fn the_span_records_the_route_template_not_the_raw_path() {
    let _serial = SERIAL.lock().await;
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

/// R-2b: the other half of the cardinality bomb — the method token.
///
/// A method is an attacker-chosen string off the unauthenticated public listener and hyper accepts
/// a 60 000-byte one, so a raw `method` label is a remote memory-exhaustion and scrape-cost attack
/// on the process and on the monitoring pipeline both, and it injects attacker text into the
/// operator plane. `ARCHITECTURE.md` S14; `THREAT_MODEL.md` "DoS/cost fan-out"; §5.4 "cardinality is
/// bounded by construction".
#[tokio::test]
async fn a_hundred_unknown_methods_produce_one_method_label() {
    let _serial = SERIAL.lock().await;
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
    let _serial = SERIAL.lock().await;
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

/// R-5: a refactor of the correlation to a `String` fails a test rather than a code review.
#[tokio::test]
async fn the_logged_correlation_is_a_parsable_contracts_entity_ref() {
    let _serial = SERIAL.lock().await;
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

/// T-5: the one middleware is attached to the public router only, so the operator plane's own
/// traffic — a scrape every fifteen seconds and whatever a person curls — neither dominates the
/// request rate nor pollutes the latency histogram.
///
/// It lives here rather than in `crates/telemetry/tests/subscriber.rs` because it needs an axum
/// router, and here rather than in `tests/admin.rs` because the capturing subscriber that proves
/// "not spanned" already exists in this file.
#[tokio::test]
async fn admin_requests_are_neither_metered_nor_spanned() {
    let _serial = SERIAL.lock().await;
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
