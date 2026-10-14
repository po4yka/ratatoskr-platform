//! Edge in front of `ratatoskr-notes` (workspace change `add-notes-vault-connections`, group 9).
//!
//! Each test reads the notes route from the shipped Edge example, points it at a loopback stub and
//! sends real requests through the public router: authentication, the minted claims, the route
//! budgets and the connect proxy.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{DeviceKind, NewSession, SecretDigest, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use uuid::Uuid;

const AUDIENCE: &str = "edge";
const OWNER_CREDENTIAL: &str = "notes-owner-credential-000000000";
const DEVICE_SECRET: &str = "notes-plugin-device-secret";
const EDGE_EXAMPLE: &str = include_str!("../../../deploy/systemd/edge.conf.example");

/// The values of the connect body that must stay inside the request.
const MARKERS: [&str; 4] = [
    "marker-email-7f3a",
    "MARKER-PASSWORD-9c1e",
    "MARKER-MFA-4b2d",
    "MARKER-E2E-6d8f",
];

/// The handlers read the wall clock themselves, so a session seeded here is minted on that clock.
fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// The value of an active `RATATOSKR__GATEWAY__ROUTES__NOTES__<key>` line of the shipped example.
fn shipped(key: &str) -> Option<String> {
    let line = format!("RATATOSKR__GATEWAY__ROUTES__NOTES__{key}=");
    EDGE_EXAMPLE
        .lines()
        .map(str::trim)
        .filter(|candidate| !candidate.starts_with('#'))
        .find_map(|candidate| candidate.strip_prefix(&line))
        .map(str::to_owned)
}

/// The notes route of the shipped Edge example, pointed at `listener` instead of 8099. The control
/// budget is small and the transfer budget large, so a test can tell the two apart.
fn notes_route(listener: std::net::SocketAddr) -> GatewayConfig {
    assert_eq!(
        shipped("PREFIX").as_deref(),
        Some("/v1/notes"),
        "the shipped Edge route table has no notes route"
    );
    assert_eq!(shipped("LISTENER").as_deref(), Some("127.0.0.1:8099"));
    assert_eq!(shipped("CLASS").as_deref(), Some("control"));
    let mut gateway = GatewayConfig {
        routes: BTreeMap::from([(
            "notes".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/notes".to_owned(),
                listener,
                class: Some(GatewayRouteClass::Control),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        )]),
        ..GatewayConfig::default()
    };
    gateway.budgets.control.max_body_bytes = 1024;
    gateway.budgets.transfer.max_body_bytes = 1_048_576;
    gateway
}

fn app(harness: &TestDatabase, gateway: &GatewayConfig) -> Router {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(gateway);
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 15,
        max_body_bytes: 4 * 1_048_576,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(Arc::new(state)),
    )
}

/// What the stub saw of one request.
struct Seen {
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

/// A notes stub that records every request. It answers a connect with the `202` of the notes
/// service and every other request with `200 {}`.
async fn stub() -> (
    std::net::SocketAddr,
    mpsc::Receiver<Seen>,
    tokio::task::JoinHandle<()>,
) {
    let (seen_tx, seen_rx) = mpsc::channel::<Seen>(8);
    let router = Router::new().fallback(move |request: axum::extract::Request| {
        let seen_tx = seen_tx.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX)
                .await
                .expect("the forwarded body");
            let connect =
                parts.method == Method::POST && parts.uri.path() == "/v1/notes/connections";
            seen_tx
                .send(Seen {
                    method: parts.method,
                    path: parts.uri.path().to_owned(),
                    headers: parts.headers,
                    body,
                })
                .await
                .expect("record the upstream request");
            if connect {
                (
                    StatusCode::ACCEPTED,
                    axum::Json(serde_json::json!({
                        "connection_id": "018f0000-0000-7000-8000-0000000000c1",
                        "channel": "sync",
                        "state": "connecting",
                    })),
                )
            } else {
                (StatusCode::OK, axum::Json(serde_json::json!({})))
            }
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the notes stub serves");
    });
    (address, seen_rx, task)
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a response body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// A user with a primary browser session that presents `OWNER_CREDENTIAL`.
async fn seed_owner(pool: &sqlx::PgPool) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user")
        .user_id;
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user,
            kind: SessionKind::Browser,
            device_id: None,
            audience: AUDIENCE,
            token: Some(auth::credential_digest(OWNER_CREDENTIAL)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user
}

/// Open a device session through the public route, as a paired client does.
async fn device_credential(app: &Router, device_id: Uuid) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/sessions/device")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"device_id":"{device_id}","device_secret":"{DEVICE_SECRET}"}}"#
        )))
        .expect("a request");
    let (status, body) = send(app, request).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["credential"]
        .as_str()
        .expect("a device credential")
        .to_owned()
}

/// Every value of `name` that the stub received.
fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .map(|value| value.to_str().expect("an ASCII header").to_owned())
        .collect()
}

#[tokio::test]
async fn device_claim_names_the_device_kind() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let kind = DeviceKind::from_str_opt("obsidian_plugin")
        .expect("obsidian_plugin is a registered device kind");
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = platform_identity::user::create_user(harness.pool(), now())
        .await
        .expect("a user")
        .user_id;
    let device = platform_identity::device::register_device(
        harness.pool(),
        owner,
        kind,
        None,
        SecretDigest::of(DEVICE_SECRET),
        now(),
    )
    .await
    .expect("a plugin device")
    .device_id;
    let app = app(&harness, &gateway);
    let credential = device_credential(&app, device).await;
    let forged = Uuid::now_v7().to_string();

    let (status, body) = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/v1/notes/connections/018f0000-0000-7000-8000-0000000000c1/changes")
            .header("authorization", format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .header("x-ratatoskr-user-id", &forged)
            .header("x-ratatoskr-device-id", &forged)
            .header("x-ratatoskr-device-kind", "export_agent")
            .body(Body::from(r#"{"changes":[]}"#))
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let request = seen.recv().await.expect("one upstream request");
    assert_eq!(
        values(&request.headers, "x-ratatoskr-user-id"),
        [owner.to_string()]
    );
    assert_eq!(
        values(&request.headers, "x-ratatoskr-device-id"),
        [device.to_string()]
    );
    assert_eq!(
        values(&request.headers, "x-ratatoskr-device-kind"),
        ["obsidian_plugin"]
    );
    assert!(request.headers.get("authorization").is_none());
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

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

#[tokio::test]
async fn blob_upload_uses_the_transfer_budget() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let bytes = vec![b'x'; 2048];
    let upload = |path: &str| {
        Request::builder()
            .method("PUT")
            .uri(format!(
                "/v1/notes/connections/018f0000-0000-7000-8000-0000000000c1/{path}"
            ))
            .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
            .header("content-length", bytes.len())
            .body(Body::from(bytes.clone()))
            .expect("a request")
    };

    let (status, body) = send(&app, upload("blobs/session-1/chunks/0")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = seen.recv().await.expect("the blob request");
    assert_eq!(request.body.len(), bytes.len());

    let (status, body) = send(&app, upload("changes")).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), seen.recv())
            .await
            .is_err(),
        "an oversized change batch reached the notes service"
    );
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
