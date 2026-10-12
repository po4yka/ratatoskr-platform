//! What Edge sends to the archive receiver, and what it cleans up afterwards (XR-021 CONTRACTS.md
//! S06 D1).
//!
//! The receiver is a loopback stub that accepts only `POST /v1/ai-archives/receipt`, which is what
//! both importers and their own tests already do. Edge used to forward `PUT`, so against a real
//! receiver every finalize ended in a 405 that Edge turned into `upstream_invalid_response`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::response::IntoResponse as _;
use axum::routing::post;
use futures_util::StreamExt as _;
use http::StatusCode;
use platform_persistence::test_support::TestDatabase;
use ratatoskr_ai_archive_contracts::platform_receipt::{
    ARCHIVE_MEDIA_TYPE, HEADER_ARCHIVE_BYTE_SIZE, HEADER_ARCHIVE_SHA256, HEADER_CORRELATION_ID,
    HEADER_DEVICE_ID, HEADER_OPERATION_ID, HEADER_USER_ID,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

mod support;

use support::{
    app, device_credential, finalize_request, prepare_and_open, put_chunk, receipt_stub,
    seed_device, send, sha256_hex, state,
};

/// Three chunks, the last one short, so the forwarded body is assembled from more than one frame.
fn multi_chunk_archive() -> Vec<u8> {
    (0..150_000_u32)
        .map(|index| u8::try_from(index % 251).expect("a byte"))
        .collect()
}

async fn upload_all(
    api: &axum::Router,
    credential: &str,
    uploads_path: &str,
    token: &str,
    archive: &[u8],
) {
    for (index, chunk) in archive.chunks(65_536).enumerate() {
        let status = put_chunk(
            api,
            credential,
            uploads_path,
            token,
            u32::try_from(index).expect("a chunk index"),
            chunk.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "chunk {index}");
    }
}

#[tokio::test]
async fn finalize_forwards_post_with_zip_content_type_and_all_claims() {
    let (sender, mut received) = mpsc::channel(2);
    let (address, task) = receipt_stub(sender).await;
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let api = app(state(&harness, address));
    let credential = device_credential(&api, device_id).await;
    let archive = multi_chunk_archive();
    let digest = sha256_hex(&archive);
    let (operation_id, uploads_path, token) = prepare_and_open(
        &api,
        &credential,
        "receipt-forward",
        &digest,
        archive.len(),
        65_536,
    )
    .await;
    upload_all(&api, &credential, &uploads_path, &token, &archive).await;

    let (status, completed) =
        send(&api, finalize_request(&uploads_path, &token, &credential)).await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["outcome"], "stored");

    let (method, headers, body) = received.recv().await.expect("one receipt delivery");
    assert_eq!(method, http::Method::POST);
    assert_eq!(headers["content-type"], ARCHIVE_MEDIA_TYPE);
    assert_eq!(headers["content-length"], archive.len().to_string());
    for claim in [
        HEADER_USER_ID,
        HEADER_DEVICE_ID,
        HEADER_CORRELATION_ID,
        HEADER_OPERATION_ID,
        HEADER_ARCHIVE_SHA256,
        HEADER_ARCHIVE_BYTE_SIZE,
    ] {
        assert!(headers.contains_key(claim), "the claim {claim} is missing");
    }
    assert_eq!(headers[HEADER_OPERATION_ID], operation_id);
    assert_eq!(headers[HEADER_DEVICE_ID], device_id.to_string());
    assert_eq!(headers[HEADER_ARCHIVE_SHA256], digest);
    assert_eq!(headers[HEADER_ARCHIVE_BYTE_SIZE], archive.len().to_string());
    assert!(
        !headers.contains_key("authorization"),
        "no credential is forwarded"
    );
    assert!(!headers.contains_key("cookie"));
    assert_eq!(body, archive, "the receiver gets the archive byte for byte");

    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn finalize_removes_the_staging_directory_after_success() {
    let (sender, mut received) = mpsc::channel(2);
    let (address, task) = receipt_stub(sender).await;
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = seed_device(harness.pool()).await;
    let staging = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/archive-staging-tests")
        .join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir_all(&staging).expect("a staging root");
    let mut api_state = state(&harness, address);
    api_state.archive_staging_root = Arc::new(staging.clone());
    let api = app(api_state);
    let credential = device_credential(&api, device_id).await;
    let archive = multi_chunk_archive();
    let (_, uploads_path, token) = prepare_and_open(
        &api,
        &credential,
        "receipt-cleanup",
        &sha256_hex(&archive),
        archive.len(),
        65_536,
    )
    .await;
    upload_all(&api, &credential, &uploads_path, &token, &archive).await;
    assert!(
        staging.join(&token).is_dir(),
        "the chunks are staged until the receiver has the bytes"
    );

    let (status, completed) =
        send(&api, finalize_request(&uploads_path, &token, &credential)).await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    received.recv().await.expect("one receipt delivery");

    assert!(
        !staging.join(&token).exists(),
        "the staging directory of a delivered archive is removed"
    );

    task.abort();
    let _ = std::fs::remove_dir_all(&staging);
    harness.cleanup().await.expect("cleanup");
}

/// A loopback receiver whose answer to `POST /v1/ai-archives/receipt` is whatever status is stored
/// in `answer` at that moment: 202 accepts, any other status is a contract `ErrorEnvelope`. It
/// drains the body first, as a real receiver does, and counts the deliveries it saw.
async fn scripted_receipt_stub(
    answer: Arc<AtomicU16>,
    deliveries: Arc<AtomicUsize>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let router = Router::new().route(
        "/v1/ai-archives/receipt",
        post(move |body: Body| {
            let answer = Arc::clone(&answer);
            let deliveries = Arc::clone(&deliveries);
            async move {
                let _ = body.into_data_stream().count().await;
                deliveries.fetch_add(1, Ordering::SeqCst);
                let status =
                    StatusCode::from_u16(answer.load(Ordering::SeqCst)).expect("a scripted status");
                if status.is_success() {
                    return (status, axum::Json(serde_json::Value::Null)).into_response();
                }
                (
                    status,
                    axum::Json(serde_json::json!({
                        "code": "chatgpt.account.unmapped",
                        "message": "This user is not set up for archive import.",
                        "retryable": false,
                    })),
                )
                    .into_response()
            }
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the scripted receipt stub serves");
    });
    (address, task)
}

/// One archive uploaded and ready to finalize against a receiver that answers `initial`.
struct Upload {
    harness: TestDatabase,
    api: axum::Router,
    credential: String,
    operation_id: String,
    uploads_path: String,
    token: String,
    staging: std::path::PathBuf,
    answer: Arc<AtomicU16>,
    deliveries: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Upload {
    async fn start(initial: u16) -> Self {
        let answer = Arc::new(AtomicU16::new(initial));
        let deliveries = Arc::new(AtomicUsize::new(0));
        let (address, task) =
            scripted_receipt_stub(Arc::clone(&answer), Arc::clone(&deliveries)).await;
        let harness = TestDatabase::create().await.expect("a test database");
        let device_id = seed_device(harness.pool()).await;
        let staging = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/archive-staging-tests")
            .join(uuid::Uuid::now_v7().simple().to_string());
        std::fs::create_dir_all(&staging).expect("a staging root");
        let mut api_state = state(&harness, address);
        api_state.archive_staging_root = Arc::new(staging.clone());
        let api = app(api_state);
        let credential = device_credential(&api, device_id).await;
        let archive = multi_chunk_archive();
        let (operation_id, uploads_path, token) = prepare_and_open(
            &api,
            &credential,
            "refusal",
            &sha256_hex(&archive),
            archive.len(),
            65_536,
        )
        .await;
        upload_all(&api, &credential, &uploads_path, &token, &archive).await;
        Self {
            harness,
            api,
            credential,
            operation_id,
            uploads_path,
            token,
            staging,
            answer,
            deliveries,
            task,
        }
    }

    async fn finalize(&self) -> (StatusCode, serde_json::Value) {
        send(
            &self.api,
            finalize_request(&self.uploads_path, &self.token, &self.credential),
        )
        .await
    }

    async fn operation(&self) -> serde_json::Value {
        let request = http::Request::builder()
            .method("GET")
            .uri(format!("/v1/operations/{}", self.operation_id))
            .header("authorization", format!("Bearer {}", self.credential))
            .body(axum::body::Body::empty())
            .expect("an operation request");
        let (status, body) = send(&self.api, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn session_state(&self) -> String {
        sqlx::query_scalar(
            "select session_state from operations.ai_archive_transfers where resumption_token = $1",
        )
        .bind(&self.token)
        .fetch_one(self.harness.pool())
        .await
        .expect("the transfer session")
    }

    fn staged(&self) -> bool {
        self.staging.join(&self.token).exists()
    }

    async fn finish(self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.staging);
        self.harness.cleanup().await.expect("cleanup");
    }
}

/// R3-03. A receiver that refuses the archive for good ends the operation in the same request, and
/// the receiver's refusal reaches the client unchanged.
#[tokio::test]
async fn a_403_from_the_receiver_ends_the_operation() {
    let upload = Upload::start(403).await;

    let (status, refusal) = upload.finalize().await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");
    assert_eq!(refusal["code"], "chatgpt.account.unmapped");

    let operation = upload.operation().await;
    assert_eq!(operation["status"], "failed", "{operation}");
    assert_eq!(operation["stage"], "delivery");
    assert_eq!(
        operation["errors"][0]["code"],
        "platform.ai_archive.delivery_failed"
    );
    assert_eq!(upload.session_state().await, "failed");
    assert!(!upload.staged(), "the staging directory is removed");

    let (second, _) = upload.finalize().await;
    assert_eq!(second, StatusCode::NOT_FOUND, "a dead token is gone");
    assert_eq!(upload.deliveries.load(Ordering::SeqCst), 1);
    upload.finish().await;
}

#[tokio::test]
async fn a_413_from_the_receiver_ends_the_operation() {
    let upload = Upload::start(413).await;

    let (status, refusal) = upload.finalize().await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{refusal}");

    let operation = upload.operation().await;
    assert_eq!(operation["status"], "failed", "{operation}");
    assert_eq!(upload.session_state().await, "failed");
    assert!(!upload.staged());
    upload.finish().await;
}

/// R5-04 rule 5. After a permanent refusal the session is dead, and opening the same operation again
/// mints a new one instead of replaying the token that now answers 404.
#[tokio::test]
async fn a_reopen_after_a_permanent_refusal_mints_a_new_session() {
    let upload = Upload::start(403).await;
    let (status, refusal) = upload.finalize().await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");
    let archive = multi_chunk_archive();

    let (status, reopened) = support::open_session(
        &upload.api,
        &upload.credential,
        &upload.uploads_path,
        &sha256_hex(&archive),
        archive.len(),
        65_536,
        ARCHIVE_MEDIA_TYPE,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{reopened}");
    let new = reopened["resumption_token"].as_str().expect("a token");
    assert_ne!(
        new, upload.token,
        "a failed session is replaced, not replayed"
    );
    assert!(upload.staging.join(new).is_dir());
    assert!(!upload.staged(), "the old directory is gone");
    upload.finish().await;
}

/// R3-03. A receiver that is merely unavailable does not end anything: the client retries finalize.
#[tokio::test]
async fn a_503_from_the_receiver_leaves_the_operation_open() {
    let upload = Upload::start(503).await;

    let (status, refusal) = upload.finalize().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{refusal}");
    assert_eq!(upload.operation().await["status"], "accepted");
    assert_eq!(upload.session_state().await, "open");
    assert!(upload.staged(), "the chunks stay for the retry");

    upload.answer.store(202, Ordering::SeqCst);
    let (status, completed) = upload.finalize().await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["outcome"], "stored");
    assert_eq!(upload.session_state().await, "finalized");
    upload.finish().await;
}

/// R3-03. A receiver's 401 is Edge's fault to explain, never the device's: it becomes a 502 and
/// ends nothing.
#[tokio::test]
async fn an_upstream_401_leaves_the_operation_open() {
    let upload = Upload::start(401).await;

    let (status, refusal) = upload.finalize().await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{refusal}");
    assert_eq!(refusal["code"], "edge.upstream_invalid_response");
    assert_eq!(upload.operation().await["status"], "accepted");
    assert_eq!(upload.session_state().await, "open");
    assert!(upload.staged());
    upload.finish().await;
}
