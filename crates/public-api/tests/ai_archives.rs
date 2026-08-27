//! AI archive acceptance through the public device surface.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::routing::put;
use futures_util::StreamExt as _;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::SecretDigest;
use platform_persistence::test_support::TestDatabase;
use platform_public_api::ApiState;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;
use uuid::Uuid;

const AUDIENCE: &str = "edge";
const DEVICE_SECRET: &str = "archive-device-secret";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn state(harness: &TestDatabase, listener: std::net::SocketAddr) -> ApiState {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(&GatewayConfig {
        routes: BTreeMap::from([(
            "chatgpt".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/chatgpt".to_owned(),
                listener,
                class: Some(GatewayRouteClass::Transfer),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        )]),
        ..GatewayConfig::default()
    });
    state
}

async fn receipt_stub(
    sender: mpsc::Sender<(http::HeaderMap, Vec<u8>)>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let router = Router::new().route(
        "/v1/ai-archives/receipt",
        put(move |headers: http::HeaderMap, body: Body| {
            let sender = sender.clone();
            async move {
                let body = body
                    .into_data_stream()
                    .fold(Vec::new(), |mut bytes, item| async move {
                        bytes.extend_from_slice(&item.expect("a streamed chunk"));
                        bytes
                    })
                    .await;
                sender
                    .send((headers, body))
                    .await
                    .expect("a receipt observation");
                StatusCode::ACCEPTED
            }
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the receipt stub serves");
    });
    (address, task)
}

fn app(state: ApiState) -> Router {
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

async fn seed_device(pool: &sqlx::PgPool) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user")
        .user_id;
    platform_identity::device::register_device(
        pool,
        user,
        platform_identity::DeviceKind::ExportAgent,
        None,
        SecretDigest::of(DEVICE_SECRET),
        now(),
    )
    .await
    .expect("a device")
    .device_id
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

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
    assert_eq!(status, StatusCode::CREATED);
    body["credential"]
        .as_str()
        .expect("a device credential")
        .to_owned()
}

#[tokio::test]
async fn configured_device_archive_preparation_creates_one_owned_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let app = app(state(
        &harness,
        "127.0.0.1:9".parse().expect("a loopback address"),
    ));
    let credential = device_credential(&app, device_id).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/chatgpt")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", "archive-prepare-1")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","byte_size":1}"#))
        .expect("a request");

    let (status, body) = send(&app, request).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        body["operation_id"]
            .as_str()
            .is_some_and(|value| Uuid::parse_str(value).is_ok()),
        "an archive preparation returns a pollable operation"
    );
    assert!(
        body["upload_path"]
            .as_str()
            .is_some_and(|value| value.starts_with("/v1/ai-archives/chatgpt/")),
        "the operation owns its streaming upload path"
    );

    let first_operation_id = body["operation_id"]
        .as_str()
        .expect("an operation id")
        .to_owned();
    let replay = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/chatgpt")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", "archive-prepare-1")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","byte_size":1}"#))
        .expect("a replay request");
    let (status, replay) = send(&app, replay).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(replay["operation_id"], first_operation_id);
    let operations: i64 =
        sqlx::query_scalar("select count(*) from operations.ai_archive_acceptances")
            .fetch_one(harness.pool())
            .await
            .expect("one archive binding");
    assert_eq!(operations, 1);

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn unconfigured_provider_creates_no_archive_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let app = app(state(
        &harness,
        "127.0.0.1:9".parse().expect("a loopback address"),
    ));
    let credential = device_credential(&app, device_id).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/claude")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", "archive-prepare-unconfigured")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"sha256":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","byte_size":1}"#))
        .expect("a request");
    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "platform.resource.not_found");
    let count: i64 = sqlx::query_scalar("select count(*) from operations.ai_archive_acceptances")
        .fetch_one(harness.pool())
        .await
        .expect("archive acceptance count");
    assert_eq!(count, 0);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn upload_forwards_only_edge_minted_archive_claims_to_the_fixed_receipt() {
    let (sender, mut received) = mpsc::channel(1);
    let (address, task) = receipt_stub(sender).await;
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let app = app(state(&harness, address));
    let credential = device_credential(&app, device_id).await;
    let digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let request = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/chatgpt")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", "archive-prepare-delivery")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"sha256":"{digest}","byte_size":3}}"#
        )))
        .expect("a request");
    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let path = body["upload_path"].as_str().expect("an upload path");
    let operation_id = body["operation_id"].as_str().expect("an operation id");

    let request = Request::builder()
        .method("PUT")
        .uri(path)
        .header("authorization", format!("Bearer {credential}"))
        .header("x-ratatoskr-operation-id", "client-forged")
        .header("x-ratatoskr-archive-sha256", "client-forged")
        .body(Body::from("zip"))
        .expect("a request");
    let (status, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (headers, bytes) = received.recv().await.expect("one importer receipt");
    assert_eq!(bytes, b"zip");
    assert_eq!(headers["x-ratatoskr-operation-id"], operation_id);
    assert_eq!(headers["x-ratatoskr-archive-sha256"], digest);
    assert_eq!(headers["x-ratatoskr-archive-byte-size"], "3");
    assert!(headers.get("authorization").is_none());
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn unavailable_importer_marks_the_known_archive_operation_failed_with_a_safe_diagnostic() {
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let app = app(state(
        &harness,
        "127.0.0.1:9".parse().expect("a loopback address"),
    ));
    let credential = device_credential(&app, device_id).await;
    let digest = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let request = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/chatgpt")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", "archive-prepare-unavailable")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"sha256":"{digest}","byte_size":3}}"#
        )))
        .expect("a request");
    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let operation_id: Uuid = body["operation_id"]
        .as_str()
        .expect("an operation id")
        .parse()
        .expect("a UUID");
    let path = body["upload_path"].as_str().expect("an upload path");

    let request = Request::builder()
        .method("PUT")
        .uri(path)
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from("zip"))
        .expect("a request");
    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "edge.upstream_unavailable");

    let operation = platform_operations::find(harness.pool(), operation_id)
        .await
        .expect("operation query")
        .expect("known operation");
    assert_eq!(
        operation.status,
        ratatoskr_operation_contracts::OperationStatus::Failed
    );
    let diagnostic: (String, String, bool) = sqlx::query_as(
        "select code, message, retryable from operations.operation_errors where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(harness.pool())
    .await
    .expect("safe delivery diagnostic");
    assert_eq!(diagnostic.0, "platform.ai_archive.delivery_failed");
    assert_eq!(diagnostic.1, "Archive delivery to the importer failed.");
    assert!(diagnostic.2);
    harness.cleanup().await.expect("cleanup");
}
