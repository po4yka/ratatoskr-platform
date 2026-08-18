//! The shutdown sequence, over real sockets — tests S-1 … S-5.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::fmt;
use std::future::pending;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use http::{StatusCode, Uri};
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use platform_core::RuntimeRole;
use platform_core::config::{PublicConfig, ShutdownConfig};
use platform_http::{
    HttpState, RuntimeState, Served, admin_router, drain_and_close, public_router, serve,
};
use tokio::net::{TcpListener, TcpStream};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// A shutdown configuration written for a test rather than for a pod.
fn timing(drain_seconds: u64, grace_seconds: u64) -> ShutdownConfig {
    ShutdownConfig {
        drain_seconds,
        grace_seconds,
    }
}

/// The admin listener of a process that has bound everything, on an ephemeral port.
async fn admin(state: &Arc<RuntimeState>) -> (Served, Uri) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = serve(listener, admin_router(Arc::clone(state), String::new));
    (served, address_to_uri(address.to_string().as_str(), "/"))
}

/// The public listener of an edge process with one slow handler, on an ephemeral port.
async fn public(state: &Arc<HttpState>, handler_seconds: u64) -> (Served, Uri) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = PublicConfig {
        bind: address,
        request_timeout_seconds: 300,
        max_body_bytes: 1024,
    };
    let routes = Router::new().route(
        "/slow",
        get(move || async move {
            tokio::time::sleep(Duration::from_secs(handler_seconds)).await;
            "done"
        }),
    );
    let served = serve(listener, public_router(Arc::clone(state), &config, routes));
    (
        served,
        address_to_uri(address.to_string().as_str(), "/slow"),
    )
}

fn address_to_uri(address: &str, path: &str) -> Uri {
    format!("http://{address}{path}").parse().unwrap()
}

fn with_path(base: &Uri, path: &str) -> Uri {
    address_to_uri(base.authority().unwrap().as_str(), path)
}

async fn status(uri: Uri) -> StatusCode {
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    client.get(uri).await.unwrap().status()
}

/// S-1: readiness fails before the listener stops accepting. The entire justification for a
/// separate readiness probe.
#[tokio::test]
async fn sigterm_makes_readiness_fail_before_the_listener_stops_accepting() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    state.mark_startup_complete();
    let http = Arc::new(HttpState::new(RuntimeRole::Edge));
    let (served, base) = admin(&state).await;
    assert_eq!(
        status(with_path(&base, "/health/ready")).await,
        StatusCode::OK
    );

    let draining = {
        let state = Arc::clone(&state);
        let http = Arc::clone(&http);
        tokio::spawn(async move {
            drain_and_close(
                &state,
                &timing(2, 5),
                vec![served],
                http.in_flight(),
                pending(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        status(with_path(&base, "/health/ready")).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "readiness must fail while the listener is still open",
    );
    assert_eq!(
        status(with_path(&base, "/health/live")).await,
        StatusCode::OK,
        "liveness must answer throughout the drain",
    );

    let outcome = draining.await.unwrap();
    assert!(outcome.graceful);
    assert!(
        TcpStream::connect(base.authority().unwrap().as_str())
            .await
            .is_err(),
        "the listener must be closed once the sequence returns",
    );
}

/// S-2: no truncated responses on deploy.
#[tokio::test]
async fn an_in_flight_request_completes_within_the_grace_window() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    state.mark_startup_complete();
    let http = Arc::new(HttpState::new(RuntimeRole::Edge));
    let (served, uri) = public(&http, 1).await;

    let request = tokio::spawn(status(uri));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let outcome = drain_and_close(
        &state,
        &timing(0, 10),
        vec![served],
        http.in_flight(),
        pending(),
    )
    .await;

    assert_eq!(request.await.unwrap(), StatusCode::OK);
    assert!(outcome.graceful);
    assert_eq!(
        outcome.in_flight_at_close, 1,
        "the request was still in flight at close"
    );
}

/// S-3: one stuck request cannot block a deploy.
///
/// The warning is asserted here; the "still exits zero" half is `run`'s, and is asserted at the
/// process level by B-1 (`services/edge/tests/boot.rs`) — `drain_and_close` returns an outcome
/// rather than an exit code.
#[tokio::test]
async fn the_grace_window_expiring_logs_a_warning_and_still_exits_zero() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    state.mark_startup_complete();
    let http = Arc::new(HttpState::new(RuntimeRole::Edge));
    let (served, uri) = public(&http, 30).await;

    let _stuck = tokio::spawn(status(uri));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let log = Captured::default();
    let capture = tracing::subscriber::set_default(Capture::installing(&log));
    let started = Instant::now();
    let outcome = drain_and_close(
        &state,
        &timing(0, 1),
        vec![served],
        http.in_flight(),
        pending(),
    )
    .await;
    drop(capture);

    assert!(!outcome.graceful, "the grace window must expire");
    assert_eq!(outcome.in_flight_at_close, 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deploy was blocked"
    );
    let log = log.text();
    assert!(
        log.contains("the grace window expired with requests still in flight"),
        "the operator was never told a request was abandoned: {log}"
    );
    assert!(
        log.contains("in_flight_at_close=1"),
        "the warning must carry the count: {log}"
    );
}

/// S-4: Ctrl-C twice works — in either window, because both are inside the sequence.
#[tokio::test]
async fn a_second_signal_short_circuits_the_drain() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    state.mark_startup_complete();
    let http = Arc::new(HttpState::new(RuntimeRole::Edge));
    let (served, _) = admin(&state).await;

    let started = Instant::now();
    let outcome = drain_and_close(
        &state,
        &timing(30, 30),
        vec![served],
        http.in_flight(),
        std::future::ready(()),
    )
    .await;

    assert!(outcome.interrupted);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a second signal must skip the drain window",
    );

    // The grace window is step 4, still inside the sequence: a second Ctrl-C while an in-flight
    // request is finishing must not be ignored for the whole of `grace_seconds`.
    let http = Arc::new(HttpState::new(RuntimeRole::Edge));
    let (served, uri) = public(&http, 30).await;
    let _stuck = tokio::spawn(status(uri));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    let outcome = drain_and_close(
        &state,
        &timing(0, 30),
        vec![served],
        http.in_flight(),
        tokio::time::sleep(Duration::from_millis(200)),
    )
    .await;

    assert!(outcome.interrupted, "the grace window ignored the signal");
    assert!(!outcome.graceful);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a second signal must skip the grace window too",
    );
}

/// S-5: drain, then close, then flush — asserted in order.
#[tokio::test]
async fn the_shutdown_sequence_order_is_drain_then_close_then_flush() {
    let state = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    state.mark_startup_complete();
    let http = Arc::new(HttpState::new(RuntimeRole::Edge));
    let (served, base) = admin(&state).await;
    let authority = base.authority().unwrap().as_str().to_owned();

    let draining = {
        let state = Arc::clone(&state);
        let http = Arc::clone(&http);
        tokio::spawn(async move {
            drain_and_close(
                &state,
                &timing(1, 5),
                vec![served],
                http.in_flight(),
                pending(),
            )
            .await
        })
    };

    // 1. Draining: readiness already fails and the socket still accepts.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!state.is_ready());
    assert!(TcpStream::connect(&authority).await.is_ok());

    // 2. Closed: the socket refuses.
    draining.await.unwrap();
    assert!(TcpStream::connect(&authority).await.is_err());

    // 3. Flushing is out of this test's scope and cannot be reached from here: `drain_and_close`
    //    does not touch telemetry, and `run` calls `TelemetryGuard::shutdown` after it returns.
    //    That step is covered by T-3 (`crates/telemetry/tests/shutdown.rs`). This test pins
    //    drain-then-close, which is the half that lives in this function.
}

/// Everything a subscriber was asked to record, one `field=value` per line.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl Captured {
    fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// A subscriber that records the field values of events. Hand-rolled because `tracing-subscriber`
/// is not a dependency of this crate, and because the property under test is what the shutdown
/// sequence *records*, not how a formatter renders it.
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

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

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
