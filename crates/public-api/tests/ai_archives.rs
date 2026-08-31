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

async fn seed_device_for_user(pool: &sqlx::PgPool, user_id: Uuid) -> Uuid {
    platform_identity::device::register_device(
        pool,
        user_id,
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

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            use core::fmt::Write as _;
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

fn assert_stored_completion(completed: &serde_json::Value, digest: &str) {
    assert_eq!(completed["outcome"], "stored");
    assert_eq!(completed["blob_ref"]["owner_service"], "ratatoskr-chatgpt");
    assert_eq!(completed["blob_ref"]["digest"]["hex"], digest);
    assert_eq!(completed["blob_ref"]["length_bytes"], 65_537);
}

async fn send_stored_completion(
    api: &Router,
    request: Request<Body>,
    digest: &str,
) -> serde_json::Value {
    let (status, completed) = send(api, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_stored_completion(&completed, digest);
    completed
}

async fn prepare_and_open(
    api: &Router,
    credential: &str,
    key: &str,
    digest: &str,
    byte_size: usize,
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
    assert_eq!(status, StatusCode::ACCEPTED);
    let operation_id = prepared["operation_id"]
        .as_str()
        .expect("an operation id")
        .to_owned();
    let uploads_path = format!("/v1/ai-archives/chatgpt/{operation_id}/uploads");
    let open = Request::builder()
        .method("POST")
        .uri(&uploads_path)
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"declared_size_bytes":{byte_size},"media_type":"application/zip","digest":{{"algorithm":"sha256","hex":"{digest}"}},"chunk_size_bytes":65536}}"#
        )))
        .expect("an open request");
    let (status, opened) = send(api, open).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = opened["resumption_token"]
        .as_str()
        .expect("a token")
        .to_owned();
    (operation_id, uploads_path, token)
}

async fn put_transfer_chunk(
    api: &Router,
    credential: &str,
    uploads_path: &str,
    token: &str,
    index: u32,
    bytes: Vec<u8>,
) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("{uploads_path}/{token}/chunks/{index}"))
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from(bytes))
        .expect("a chunk request");
    assert_eq!(send(api, request).await.0, StatusCode::OK);
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
async fn provider_without_live_report_path_refuses_archive_preparation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let api_state = state(&harness, "127.0.0.1:9".parse().expect("a loopback address"));
    api_state.health.set_archive_report_ready("chatgpt", false);
    let api = app(api_state.clone());
    let credential = device_credential(&api, device_id).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/chatgpt")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", "archive-unready-report")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","byte_size":1}"#))
        .expect("a request");

    assert_eq!(send(&api, request).await.0, StatusCode::NOT_FOUND);
    assert!(!api_state.health.archive_provider_ready("chatgpt"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from operations.ai_archive_acceptances")
            .fetch_one(harness.pool())
            .await
            .expect("the archive acceptance count"),
        0
    );
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn operation_bound_upload_resumes_missing_chunks_without_second_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let api_state = state(&harness, "127.0.0.1:9".parse().expect("a loopback address"));
    let initial = app(api_state.clone());
    let credential = device_credential(&initial, device_id).await;
    let digest = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let prepare = Request::builder()
        .method("POST")
        .uri("/v1/ai-archives/chatgpt")
        .header("authorization", format!("Bearer {credential}"))
        .header("idempotency-key", "archive-resume-operation")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"sha256":"{digest}","byte_size":65537}}"#
        )))
        .expect("a prepare request");
    let (status, prepared) = send(&initial, prepare).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let operation_id = prepared["operation_id"].as_str().expect("an operation id");
    let uploads_path = format!("/v1/ai-archives/chatgpt/{operation_id}/uploads");
    let open = Request::builder()
        .method("POST")
        .uri(&uploads_path)
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"declared_size_bytes":65537,"media_type":"application/zip","digest":{{"algorithm":"sha256","hex":"{digest}"}},"chunk_size_bytes":65536}}"#
        )))
        .expect("an open request");
    let (status, opened) = send(&initial, open).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the operation-scoped upload session must exist"
    );
    let token = opened["resumption_token"]
        .as_str()
        .expect("a resumption token");

    let chunk_path = format!("{uploads_path}/{token}/chunks/0");
    let chunk = Request::builder()
        .method("PUT")
        .uri(chunk_path)
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from(vec![b'x'; 65_536]))
        .expect("a chunk request");
    let (status, receipt) = send(&initial, chunk).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["received_chunks_count"], 1);

    let restarted = app(api_state);
    let status_request = Request::builder()
        .method("GET")
        .uri(format!("{uploads_path}/{token}/status"))
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a status request");
    let (status, resumed) = send(&restarted, status_request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resumed["received_chunks"], serde_json::json!([0]));
    assert_eq!(resumed["missing_chunks_count"], 1);

    let final_chunk = Request::builder()
        .method("PUT")
        .uri(format!("{uploads_path}/{token}/chunks/1"))
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from("y"))
        .expect("a final chunk request");
    let (status, receipt) = send(&restarted, final_chunk).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["received_chunks_count"], 2);

    let operations: i64 = sqlx::query_scalar(
        "select count(*) from operations.ai_archive_acceptances where operation_id = $1",
    )
    .bind(Uuid::parse_str(operation_id).expect("an operation UUID"))
    .fetch_one(harness.pool())
    .await
    .expect("the original operation binding");
    assert_eq!(operations, 1, "resume must not create another operation");
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one authority matrix proves all owner, device, provider and replay boundaries"
)]
async fn operation_transfer_is_idempotent_and_owner_device_provider_scoped() {
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let api_state = state(&harness, "127.0.0.1:9".parse().expect("a loopback address"));
    let api = app(api_state);
    let credential = device_credential(&api, device_id).await;
    let digest = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    let prepare_request = || {
        Request::builder()
            .method("POST")
            .uri("/v1/ai-archives/chatgpt")
            .header("authorization", format!("Bearer {credential}"))
            .header("idempotency-key", "archive-idempotent-scope")
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"sha256":"{digest}","byte_size":65536}}"#
            )))
            .expect("a prepare request")
    };
    let (status, prepared) = send(&api, prepare_request()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let operation_id = prepared["operation_id"]
        .as_str()
        .expect("an operation id")
        .to_owned();
    let (status, replayed) = send(&api, prepare_request()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(replayed["operation_id"], operation_id);

    let uploads_path = format!("/v1/ai-archives/chatgpt/{operation_id}/uploads");
    let open_request = || {
        Request::builder()
            .method("POST")
            .uri(&uploads_path)
            .header("authorization", format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"declared_size_bytes":65536,"media_type":"application/zip","digest":{{"algorithm":"sha256","hex":"{digest}"}},"chunk_size_bytes":65536}}"#
            )))
            .expect("an open request")
    };
    let (status, opened) = send(&api, open_request()).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = opened["resumption_token"]
        .as_str()
        .expect("a resumption token")
        .to_owned();
    let (status, reopened) = send(&api, open_request()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        reopened["resumption_token"], token,
        "identical open must recover the durable session"
    );

    let chunk_path = format!("{uploads_path}/{token}/chunks/0");
    let chunk_request = |byte| {
        Request::builder()
            .method("PUT")
            .uri(&chunk_path)
            .header("authorization", format!("Bearer {credential}"))
            .body(Body::from(vec![byte; 65_536]))
            .expect("a chunk request")
    };
    let (status, first) = send(&api, chunk_request(b'a')).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["idempotent_replay"], false);
    let (status, replay) = send(&api, chunk_request(b'a')).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["idempotent_replay"], true);
    let (status, _) = send(&api, chunk_request(b'b')).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let owner_id: Uuid = sqlx::query_scalar(
        "select owner_user_id from operations.ai_archive_acceptances where operation_id = $1",
    )
    .bind(Uuid::parse_str(&operation_id).expect("operation UUID"))
    .fetch_one(harness.pool())
    .await
    .expect("the owner");
    let other_device = seed_device_for_user(harness.pool(), owner_id).await;
    let other_credential = device_credential(&api, other_device).await;
    let wrong_device = Request::builder()
        .method("GET")
        .uri(format!("{uploads_path}/{token}/status"))
        .header("authorization", format!("Bearer {other_credential}"))
        .body(Body::empty())
        .expect("a wrong-device request");
    assert_eq!(send(&api, wrong_device).await.0, StatusCode::NOT_FOUND);
    let wrong_provider = Request::builder()
        .method("GET")
        .uri(format!(
            "/v1/ai-archives/claude/{operation_id}/uploads/{token}/status"
        ))
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a wrong-provider request");
    assert_eq!(send(&api, wrong_provider).await.0, StatusCode::NOT_FOUND);

    let mut transaction = harness.pool().begin().await.expect("a transaction");
    platform_identity::device::revoke_device(&mut transaction, device_id, now())
        .await
        .expect("device revocation");
    transaction.commit().await.expect("revocation commit");
    let revoked = Request::builder()
        .method("GET")
        .uri(format!("{uploads_path}/{token}/status"))
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a revoked request");
    assert_eq!(send(&api, revoked).await.0, StatusCode::UNAUTHORIZED);
    let chunks: i64 = sqlx::query_scalar(
        "select count(*) from operations.ai_archive_transfer_chunks where resumption_token = $1",
    )
    .bind(&token)
    .fetch_one(harness.pool())
    .await
    .expect("chunk count");
    assert_eq!(chunks, 1, "refusals cannot change stored chunks");
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn archive_finalization_verifies_before_bound_provider_delivery() {
    let (sender, mut received) = mpsc::channel(2);
    let (address, task) = receipt_stub(sender).await;
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let api = app(state(&harness, address));
    let credential = device_credential(&api, device_id).await;
    let mut archive = vec![b'a'; 65_536];
    archive.push(b'z');

    let (_, mismatch_path, mismatch_token) = prepare_and_open(
        &api,
        &credential,
        "archive-finalize-mismatch",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        archive.len(),
    )
    .await;
    put_transfer_chunk(
        &api,
        &credential,
        &mismatch_path,
        &mismatch_token,
        0,
        archive[..65_536].to_vec(),
    )
    .await;
    put_transfer_chunk(
        &api,
        &credential,
        &mismatch_path,
        &mismatch_token,
        1,
        vec![b'z'],
    )
    .await;
    let mismatch_finalize = Request::builder()
        .method("POST")
        .uri(format!("{mismatch_path}/{mismatch_token}/finalize"))
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"resumption_token":"{mismatch_token}"}}"#
        )))
        .expect("a finalize request");
    let (status, mismatch) = send(&api, mismatch_finalize).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mismatch["outcome"], "digest_mismatch");
    assert!(
        received.try_recv().is_err(),
        "digest mismatch cannot call an upstream"
    );

    let digest = sha256_hex(&archive);
    let (operation_id, uploads_path, token) = prepare_and_open(
        &api,
        &credential,
        "archive-finalize-delivery",
        &digest,
        archive.len(),
    )
    .await;
    put_transfer_chunk(&api, &credential, &uploads_path, &token, 1, vec![b'z']).await;
    put_transfer_chunk(
        &api,
        &credential,
        &uploads_path,
        &token,
        0,
        archive[..65_536].to_vec(),
    )
    .await;
    let finalize = Request::builder()
        .method("POST")
        .uri(format!("{uploads_path}/{token}/finalize"))
        .header("authorization", format!("Bearer {credential}"))
        .header("x-ratatoskr-operation-id", "client-forged")
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"resumption_token":"{token}"}}"#)))
        .expect("a finalize request");
    let completed = send_stored_completion(&api, finalize, &digest).await;
    let (headers, forwarded) = received.recv().await.expect("one provider delivery");
    assert_eq!(forwarded, archive);
    assert_eq!(headers["x-ratatoskr-operation-id"], operation_id);
    assert_eq!(headers["x-ratatoskr-archive-sha256"], digest);
    assert_eq!(headers["x-ratatoskr-archive-byte-size"], "65537");
    let retry = Request::builder()
        .method("POST")
        .uri(format!("{uploads_path}/{token}/finalize"))
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"resumption_token":"{token}"}}"#)))
        .expect("a finalize retry");
    let retry_completed = send_stored_completion(&api, retry, &digest).await;
    assert_eq!(retry_completed, completed);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), received.recv())
            .await
            .is_err(),
        "a finalized transfer retry must not redeliver bytes"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}
