//! The shutdown sequence, over real sockets — tests S-1, S-2, S-4 and S-5. S-3 captures the log and
//! is in `shutdown_log.rs`; that file says why.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod shutdown_support;

use std::future::pending;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::StatusCode;
use platform_core::RuntimeRole;
use platform_http::{HttpState, RuntimeState, drain_and_close};
use shutdown_support::{admin, public, status, timing, with_path};
use tokio::net::TcpStream;

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
