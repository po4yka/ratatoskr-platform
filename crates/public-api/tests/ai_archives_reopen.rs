//! What opening an upload session again does to a session that is dead, live, delivered or expired
//! (XR-021 CONTRACTS.md R5-04 rule 5).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode};
use platform_persistence::test_support::TestDatabase;
use tokio::sync::mpsc;

mod support;

use support::{
    app, device_credential, finalize_request, open_session, prepare_and_open, put_chunk,
    receipt_stub, seed_device, send, sha256_hex, state,
};

/// Two chunks: one full and one of a single byte.
fn archive() -> Vec<u8> {
    let mut bytes = vec![b'a'; 65_536];
    bytes.push(b'z');
    bytes
}

const WRONG_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// An Edge with a private staging root, one device and a receiver that accepts everything.
struct Edge {
    harness: TestDatabase,
    api: Router,
    credential: String,
    staging: PathBuf,
    receipts: mpsc::Receiver<support::Observation>,
    stub: tokio::task::JoinHandle<()>,
}

impl Edge {
    async fn start() -> Self {
        let (sender, receipts) = mpsc::channel(4);
        let (address, stub) = receipt_stub(sender).await;
        let harness = TestDatabase::create().await.expect("a test database");
        let device_id = seed_device(harness.pool()).await;
        let staging = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/archive-staging-tests")
            .join(uuid::Uuid::now_v7().simple().to_string());
        std::fs::create_dir_all(&staging).expect("a staging root");
        let mut api_state = state(&harness, address);
        api_state.archive_staging_root = Arc::new(staging.clone());
        let api = app(api_state);
        let credential = device_credential(&api, device_id).await;
        Self {
            harness,
            api,
            credential,
            staging,
            receipts,
            stub,
        }
    }

    /// Prepare and open an upload of `archive()` that declares `digest`, then send both chunks.
    async fn upload(&self, key: &str, digest: &str) -> (String, String) {
        let bytes = archive();
        let (_, uploads_path, token) = prepare_and_open(
            &self.api,
            &self.credential,
            key,
            digest,
            bytes.len(),
            65_536,
        )
        .await;
        for (index, chunk) in bytes.chunks(65_536).enumerate() {
            let status = put_chunk(
                &self.api,
                &self.credential,
                &uploads_path,
                &token,
                u32::try_from(index).expect("an index"),
                chunk.to_vec(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        (uploads_path, token)
    }

    async fn reopen(&self, uploads_path: &str, digest: &str) -> (StatusCode, serde_json::Value) {
        open_session(
            &self.api,
            &self.credential,
            uploads_path,
            digest,
            archive().len(),
            65_536,
            "application/zip",
        )
        .await
    }

    async fn status_of(&self, uploads_path: &str, token: &str) -> StatusCode {
        let request = Request::builder()
            .method("GET")
            .uri(format!("{uploads_path}/{token}/status"))
            .header("authorization", format!("Bearer {}", self.credential))
            .body(Body::empty())
            .expect("a status request");
        send(&self.api, request).await.0
    }

    async fn finish(self) {
        self.stub.abort();
        let _ = std::fs::remove_dir_all(&self.staging);
        self.harness.cleanup().await.expect("cleanup");
    }
}

#[tokio::test]
async fn a_reopen_after_a_digest_mismatch_mints_a_new_session() {
    let edge = Edge::start().await;
    let (uploads_path, old) = edge.upload("reopen-mismatch", WRONG_DIGEST).await;
    let (status, outcome) = send(
        &edge.api,
        finalize_request(&uploads_path, &old, &edge.credential),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome["outcome"], "digest_mismatch");
    assert!(
        edge.staging.join(&old).is_dir(),
        "a digest mismatch leaves the chunks on disk"
    );

    let (status, reopened) = edge.reopen(&uploads_path, WRONG_DIGEST).await;

    assert_eq!(status, StatusCode::CREATED, "{reopened}");
    let new = reopened["resumption_token"]
        .as_str()
        .expect("a token")
        .to_owned();
    assert_ne!(new, old, "a failed session is replaced, not replayed");
    assert!(
        !edge.staging.join(&old).exists(),
        "the old directory is gone"
    );
    assert!(edge.staging.join(&new).is_dir(), "the new one exists");
    assert_eq!(
        edge.status_of(&uploads_path, &old).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(edge.status_of(&uploads_path, &new).await, StatusCode::OK);
    edge.finish().await;
}

#[tokio::test]
async fn an_open_unexpired_session_replays_its_token() {
    let edge = Edge::start().await;
    let (uploads_path, token) = edge.upload("reopen-open", WRONG_DIGEST).await;

    let (status, reopened) = edge.reopen(&uploads_path, WRONG_DIGEST).await;

    assert_eq!(status, StatusCode::CREATED, "{reopened}");
    assert_eq!(reopened["resumption_token"], token);
    assert!(edge.staging.join(&token).is_dir(), "its chunks are kept");
    edge.finish().await;
}

#[tokio::test]
async fn a_finalized_session_replays_its_token_and_finalize_returns_the_stored_completion() {
    let mut edge = Edge::start().await;
    let digest = sha256_hex(&archive());
    let (uploads_path, token) = edge.upload("reopen-finalized", &digest).await;
    let (status, completed) = send(
        &edge.api,
        finalize_request(&uploads_path, &token, &edge.credential),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["outcome"], "stored");
    edge.receipts.recv().await.expect("one delivery");

    let (status, reopened) = edge.reopen(&uploads_path, &digest).await;

    assert_eq!(status, StatusCode::CREATED, "{reopened}");
    assert_eq!(reopened["resumption_token"], token);
    let (status, again) = send(
        &edge.api,
        finalize_request(&uploads_path, &token, &edge.credential),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again, completed, "the stored completion is returned");
    edge.finish().await;
}

#[tokio::test]
async fn an_expired_session_is_replaced() {
    let edge = Edge::start().await;
    let (uploads_path, old) = edge.upload("reopen-expired", WRONG_DIGEST).await;
    sqlx::query(
        "update operations.ai_archive_transfers
            set opened_at = now() - interval '25 hours', expires_at = now() - interval '1 hour'
          where resumption_token = $1",
    )
    .bind(&old)
    .execute(edge.harness.pool())
    .await
    .expect("an expired session");

    let (status, reopened) = edge.reopen(&uploads_path, WRONG_DIGEST).await;

    assert_eq!(status, StatusCode::CREATED, "{reopened}");
    let new = reopened["resumption_token"].as_str().expect("a token");
    assert_ne!(new, old);
    assert!(!edge.staging.join(&old).exists());
    assert!(edge.staging.join(new).is_dir());
    edge.finish().await;
}
