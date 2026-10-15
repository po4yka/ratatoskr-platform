//! The shutdown test that captures the log — S-3.
//!
//! It is alone in its test binary on purpose. It records the log through a scoped subscriber
//! (`tracing::subscriber::set_default`). While that subscriber is the only registered dispatcher,
//! `tracing-core` computes the interest of a callsite that another thread registers first from the
//! default dispatcher of THAT thread, which is no subscriber, and caches `never` for every thread.
//! Another shutdown test that reaches the same warning of `drain_and_close` first on another thread
//! would disable it here. In a binary of its own, every callsite of the sequence is registered on
//! the thread of this test. A test that does not capture the log belongs in `shutdown.rs`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod shutdown_support;

use std::fmt;
use std::future::pending;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use platform_core::RuntimeRole;
use platform_http::{HttpState, RuntimeState, drain_and_close};
use shutdown_support::{public, status, timing};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

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
