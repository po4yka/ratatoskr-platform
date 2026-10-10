//! Helpers shared by the AI archive test binaries: a device, a gateway to a receipt stub, and the
//! prepare, open and chunk requests of the upload protocol.

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
use axum::routing::post;
use futures_util::StreamExt as _;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{
    GatewayConfig, GatewayRouteBudget, GatewayRouteBudgets, GatewayRouteClass, GatewayRouteConfig,
    PublicConfig,
};
use platform_http::{HttpState, RuntimeState};
use platform_identity::SecretDigest;
use platform_persistence::test_support::TestDatabase;
use platform_public_api::ApiState;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;
use uuid::Uuid;

pub(crate) const AUDIENCE: &str = "edge";
pub(crate) const DEVICE_SECRET: &str = "archive-device-secret";

/// The handlers read the wall clock themselves, so a session seeded here is minted on that clock.
pub(crate) fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// An Edge whose `chatgpt` route points at `listener` and whose transfer budget is
/// `transfer_budget_bytes`.
pub(crate) fn state_with_budget(
    harness: &TestDatabase,
    listener: std::net::SocketAddr,
    transfer_budget_bytes: u64,
) -> ApiState {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    health.set_archive_staging_ready(true);
    health.set_archive_receipt_ready("chatgpt", true);
    health.set_archive_report_ready("chatgpt", true);
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
        budgets: GatewayRouteBudgets {
            transfer: GatewayRouteBudget {
                max_body_bytes: transfer_budget_bytes,
                response_timeout_seconds: 300,
            },
            ..GatewayRouteBudgets::default()
        },
    });
    state
}

/// The default test Edge: a 100 MiB transfer budget.
pub(crate) fn state(harness: &TestDatabase, listener: std::net::SocketAddr) -> ApiState {
    state_with_budget(harness, listener, 104_857_600)
}

/// The public router with a request body limit of `max_body_bytes`.
pub(crate) fn app_with_limit(state: ApiState, max_body_bytes: u64) -> Router {
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 15,
        max_body_bytes,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(Arc::new(state)),
    )
}

/// The default test router: a 4 MiB body limit.
pub(crate) fn app(state: ApiState) -> Router {
    app_with_limit(state, 4 * 1_048_576)
}

/// What the receipt stub saw: the method, the headers and the whole body.
pub(crate) type Observation = (http::Method, http::HeaderMap, Vec<u8>);

/// A loopback receiver that accepts `POST /v1/ai-archives/receipt` (and nothing else: any other
/// method is the router's own 405) and reports what it received.
pub(crate) async fn receipt_stub(
    sender: mpsc::Sender<Observation>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let router = Router::new().route(
        "/v1/ai-archives/receipt",
        post(
            move |method: http::Method, headers: http::HeaderMap, body: Body| {
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
                        .send((method, headers, body))
                        .await
                        .expect("a receipt observation");
                    StatusCode::ACCEPTED
                }
            },
        ),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the receipt stub serves");
    });
    (address, task)
}

pub(crate) async fn seed_device(pool: &sqlx::PgPool) -> Uuid {
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

pub(crate) async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
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
    assert_eq!(status, StatusCode::CREATED);
    body["credential"]
        .as_str()
        .expect("a device credential")
        .to_owned()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            use core::fmt::Write as _;
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

/// Prepare an archive of `byte_size` bytes with `digest` and open its upload session with chunks of
/// `chunk_size_bytes`. Returns the operation id, the uploads path and the resumption token.
pub(crate) async fn prepare_and_open(
    api: &Router,
    credential: &str,
    key: &str,
    digest: &str,
    byte_size: usize,
    chunk_size_bytes: u32,
) -> (String, String, String) {
    let prepare = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/chatgpt")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", key)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"sha256":"{digest}","byte_size":{byte_size}}}"#
        )))
        .expect("a prepare request");
    let (status, prepared) = send(api, prepare).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{prepared}");
    let operation_id = prepared["operation_id"]
        .as_str()
        .expect("an operation id")
        .to_owned();
    let uploads_path = format!("/v1/ai-archives/chatgpt/{operation_id}/uploads");
    let (status, opened) = open_session(
        api,
        credential,
        &uploads_path,
        digest,
        byte_size,
        chunk_size_bytes,
        "application/zip",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let token = opened["resumption_token"]
        .as_str()
        .expect("a token")
        .to_owned();
    (operation_id, uploads_path, token)
}

/// `POST .../uploads` with an arbitrary declared media type.
pub(crate) async fn open_session(
    api: &Router,
    credential: &str,
    uploads_path: &str,
    digest: &str,
    byte_size: usize,
    chunk_size_bytes: u32,
    media_type: &str,
) -> (StatusCode, serde_json::Value) {
    let open = Request::builder()
        .method("POST")
        .uri(uploads_path)
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"declared_size_bytes":{byte_size},"media_type":"{media_type}","digest":{{"algorithm":"sha256","hex":"{digest}"}},"chunk_size_bytes":{chunk_size_bytes}}}"#
        )))
        .expect("an open request");
    send(api, open).await
}

pub(crate) async fn put_chunk(
    api: &Router,
    credential: &str,
    uploads_path: &str,
    token: &str,
    index: u32,
    bytes: Vec<u8>,
) -> StatusCode {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("{uploads_path}/{token}/chunks/{index}"))
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from(bytes))
        .expect("a chunk request");
    send(api, request).await.0
}

pub(crate) fn finalize_request(uploads_path: &str, token: &str, credential: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("{uploads_path}/{token}/finalize"))
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"resumption_token":"{token}"}}"#)))
        .expect("a finalize request")
}
