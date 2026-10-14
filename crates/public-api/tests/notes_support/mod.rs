//! Helpers shared by the notes test binaries: the notes route of the shipped Edge example, the
//! public router, an owner session, stub notes upstreams and the connect operation.

#![allow(
    dead_code,
    reason = "each notes test binary uses a part of these helpers"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tokio::net::TcpListener;
use tower::ServiceExt as _;
use uuid::Uuid;

pub(crate) const AUDIENCE: &str = "edge";
pub(crate) const OWNER_CREDENTIAL: &str = "notes-owner-credential-000000000";
const EDGE_EXAMPLE: &str = include_str!("../../../../deploy/systemd/edge.conf.example");

/// The fixed message of every error that Edge records on a failed connect.
pub(crate) const CONNECT_FAILED: &str = "The connect did not complete.";

/// The handlers read the wall clock themselves, so a session seeded here is minted on that clock.
pub(crate) fn now() -> jiff::Timestamp {
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
pub(crate) fn notes_route(listener: std::net::SocketAddr) -> GatewayConfig {
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

pub(crate) fn app(harness: &TestDatabase, gateway: &GatewayConfig) -> Router {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(gateway);
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 15,
        // At least the transfer budget, as the configuration validation requires.
        max_body_bytes: gateway.budgets.transfer.max_body_bytes.max(4 * 1_048_576),
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(Arc::new(state)),
    )
}

pub(crate) async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = send_with_headers(app, request).await;
    (status, body)
}

/// Like [`send`], and also the answer headers.
pub(crate) async fn send_with_headers(
    app: &Router,
    request: Request<Body>,
) -> (StatusCode, http::HeaderMap, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a response body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, body)
}

/// A user with a primary browser session that presents `OWNER_CREDENTIAL`.
pub(crate) async fn seed_owner(pool: &sqlx::PgPool) -> Uuid {
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

/// A notes stub that reads the body of every request, as far as it arrives, and then answers with
/// `status`, `headers` and `body` after `delay`.
pub(crate) async fn answering_with(
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: serde_json::Value,
    delay: Duration,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(move |request: axum::extract::Request| {
        let body = body.clone();
        let headers = headers.clone();
        async move {
            let _ = axum::body::to_bytes(request.into_body(), usize::MAX).await;
            tokio::time::sleep(delay).await;
            let mut answer =
                axum::response::IntoResponse::into_response((status, axum::Json(body)));
            for (name, value) in headers {
                answer
                    .headers_mut()
                    .insert(name, http::HeaderValue::from_static(value));
            }
            answer
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
    (address, task)
}

/// A notes stub that answers every request with `status` and `body` after `delay`.
pub(crate) async fn answering(
    status: StatusCode,
    body: serde_json::Value,
    delay: Duration,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    answering_with(status, Vec::new(), body, delay).await
}

/// A connect with a small body, as the owner.
pub(crate) fn owner_connect() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/notes/connections")
        .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"channel":"sync"}"#))
        .expect("a request")
}

/// The one connect operation of `owner`, as a client reads it.
pub(crate) async fn connect_operation(
    pool: &sqlx::PgPool,
    owner: Uuid,
) -> ratatoskr_operation_contracts::OperationSnapshot {
    let operation_id: Uuid = sqlx::query_scalar(
        "select operation_id from operations.operations
          where owner_user_id = $1 and kind = 'notes.connection.connect'",
    )
    .bind(owner)
    .fetch_one(pool)
    .await
    .expect("exactly one connect operation");
    platform_operations::snapshot(pool, operation_id)
        .await
        .expect("the operation snapshot")
}

/// Assert that the one connect operation of `owner` is `failed` with the one error `code` and the
/// fixed message.
pub(crate) async fn assert_failed_with(pool: &sqlx::PgPool, owner: Uuid, code: &str) {
    let operation = connect_operation(pool, owner).await;
    assert_eq!(
        operation.status,
        ratatoskr_operation_contracts::OperationStatus::Failed,
        "the connect operation is not terminated"
    );
    let errors: Vec<(&str, &str)> = operation
        .errors
        .iter()
        .map(|error| (error.code.as_str(), error.message.as_str()))
        .collect();
    assert_eq!(errors, [(code, CONNECT_FAILED)]);
}

/// Send `request` through `gateway` and assert the answer and the terminal state of the operation.
pub(crate) async fn assert_connect_fails(
    gateway: &GatewayConfig,
    request: Request<Body>,
    answer: StatusCode,
    code: &str,
) {
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, gateway);

    let (status, body) = send(&app, request).await;

    assert_eq!(status, answer, "{body}");
    assert_failed_with(harness.pool(), owner, code).await;
    harness.cleanup().await.expect("cleanup");
}

/// The secret of every plugin device that a test registers.
pub(crate) const DEVICE_SECRET: &str = "notes-plugin-device-secret";

/// What the stub saw of one request.
pub(crate) struct Seen {
    pub(crate) method: http::Method,
    pub(crate) path: String,
    pub(crate) query: Option<String>,
    pub(crate) headers: http::HeaderMap,
    pub(crate) body: axum::body::Bytes,
}

/// A notes stub that records every request. It answers a connect with the `202` of the notes
/// service and every other request with `200 {}`.
pub(crate) async fn stub() -> (
    std::net::SocketAddr,
    tokio::sync::mpsc::Receiver<Seen>,
    tokio::task::JoinHandle<()>,
) {
    let (seen_tx, seen_rx) = tokio::sync::mpsc::channel::<Seen>(8);
    let router = Router::new().fallback(move |request: axum::extract::Request| {
        let seen_tx = seen_tx.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX)
                .await
                .expect("the forwarded body");
            let connect =
                parts.method == http::Method::POST && parts.uri.path() == "/v1/notes/connections";
            seen_tx
                .send(Seen {
                    method: parts.method,
                    path: parts.uri.path().to_owned(),
                    query: parts.uri.query().map(str::to_owned),
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

/// A user with a registered `obsidian_plugin` device whose secret is [`DEVICE_SECRET`]: the owner
/// and the device.
pub(crate) async fn seed_plugin(pool: &sqlx::PgPool) -> (Uuid, Uuid) {
    let owner = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user")
        .user_id;
    let kind = platform_identity::DeviceKind::from_str_opt("obsidian_plugin")
        .expect("obsidian_plugin is a registered device kind");
    let device = platform_identity::device::register_device(
        pool,
        owner,
        kind,
        None,
        platform_identity::SecretDigest::of(DEVICE_SECRET),
        now(),
    )
    .await
    .expect("a plugin device")
    .device_id;
    (owner, device)
}

/// Open a device session through the public route, as a paired client does.
pub(crate) async fn device_credential(app: &Router, device_id: Uuid) -> String {
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
pub(crate) fn values(headers: &http::HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .map(|value| value.to_str().expect("an ASCII header").to_owned())
        .collect()
}
