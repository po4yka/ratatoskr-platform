//! The connect call of `ratatoskr-notes` at Edge keeps no part of its body (workspace change
//! `add-notes-vault-connections`, tasks 9.1, 9.2 and 9.32).
//!
//! This test is alone in its test binary on purpose. It records the log through a scoped
//! subscriber (`tracing::subscriber::set_default`). While that subscriber is the only registered
//! dispatcher, `tracing-core` computes the interest of a callsite that another thread registers
//! first from the default dispatcher of THAT thread, which is no subscriber, and caches `never` for
//! every thread. A test of the same binary that sends the first request through the public router
//! on another thread at that time disables the request span of this test, and the capture proves
//! nothing. In a binary of its own, every callsite of the request is registered on the thread of
//! this test, whose default dispatcher is the capture.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod notes_support;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use http::{Method, Request, StatusCode};
use notes_support::{OWNER_CREDENTIAL, app, notes_route, seed_owner, send, stub, values};
use platform_persistence::test_support::TestDatabase;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use uuid::Uuid;

/// The values of the connect body that must stay inside the request.
const MARKERS: [&str; 4] = [
    "marker-email-7f3a",
    "MARKER-PASSWORD-9c1e",
    "MARKER-MFA-4b2d",
    "MARKER-E2E-6d8f",
];

#[tokio::test]
async fn connect_body_is_not_persisted_or_logged() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let sent = serde_json::to_vec(&serde_json::json!({
        "channel": "sync",
        "vault_key": "0123456789abcdef0123456789abcdef",
        "vault_display_name": "Notes",
        "sync_login": {
            "email": format!("{}@example.invalid", MARKERS[0]),
            "password": MARKERS[1],
            "mfa_code": MARKERS[2],
            "e2e_password": MARKERS[3],
        },
    }))
    .expect("a connect body");

    let log = Captured::default();
    let guard = tracing::subscriber::set_default(Capture::installing(&log));
    let (status, answer) = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/v1/notes/connections")
            .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
            .header("content-type", "application/json")
            .header("idempotency-key", "connect-vault-1")
            .body(Body::from(sent.clone()))
            .expect("a request"),
    )
    .await;
    let request = tokio::time::timeout(Duration::from_secs(5), seen.recv())
        .await
        .expect("the upstream request arrives")
        .expect("one upstream request");
    drop(guard);

    assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
    let operation_id = answer["operation_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(|| panic!("the answer names the minted operation: {answer}"));
    assert_eq!(answer["status"], "accepted");
    assert_eq!(
        answer["connection"]["connection_id"],
        "018f0000-0000-7000-8000-0000000000c1"
    );
    assert_eq!(request.method, Method::POST);
    assert_eq!(request.path, "/v1/notes/connections");
    assert_eq!(request.body.as_ref(), sent.as_slice(), "the body changed");
    assert_eq!(
        values(&request.headers, "x-ratatoskr-operation-id"),
        [operation_id.to_string()]
    );

    let log = log.text();
    assert!(
        log.contains("/v1/notes/connections"),
        "the capture recorded no request span, so it proves nothing: {log}"
    );
    for marker in MARKERS {
        assert!(!log.contains(marker), "the log holds {marker}: {log}");
    }
    let tables: Vec<(String, String)> = sqlx::query_as(
        "select table_schema::text, table_name::text from information_schema.tables
          where table_type = 'BASE TABLE'
            and table_schema not in ('pg_catalog', 'information_schema')",
    )
    .fetch_all(harness.pool())
    .await
    .expect("the table list");
    for required in [
        "operations.outbox",
        "operations.idempotency_records",
        "identity.audit_events",
    ] {
        assert!(
            tables
                .iter()
                .any(|(schema, table)| format!("{schema}.{table}") == required),
            "the scan misses {required}"
        );
    }
    for (schema, table) in &tables {
        for marker in MARKERS {
            let (found,): (i64,) = sqlx::query_as(&format!(
                r#"select count(*) from "{schema}"."{table}" as row where row::text like '%' || $1 || '%'"#
            ))
            .bind(marker)
            .fetch_one(harness.pool())
            .await
            .expect("the table scan");
            assert_eq!(found, 0, "{schema}.{table} holds {marker}");
        }
    }
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// Everything a subscriber was asked to record, one `field=value` per line.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl Captured {
    fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// A subscriber that records every field value of every span and event on this thread.
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
