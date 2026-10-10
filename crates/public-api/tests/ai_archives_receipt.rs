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

use http::StatusCode;
use platform_persistence::test_support::TestDatabase;
use ratatoskr_ai_archive_contracts::platform_receipt::{
    ARCHIVE_MEDIA_TYPE, HEADER_ARCHIVE_BYTE_SIZE, HEADER_ARCHIVE_SHA256, HEADER_CORRELATION_ID,
    HEADER_DEVICE_ID, HEADER_OPERATION_ID, HEADER_USER_ID,
};
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
