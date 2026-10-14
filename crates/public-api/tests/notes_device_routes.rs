//! The device routes and the read routes of `ratatoskr-notes` at Edge (workspace change
//! `add-notes-vault-connections`, tasks 9.18 onward): the body limits and the budget of each route.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod notes_support;

use std::time::Duration;

use axum::body::Body;
use http::{Method, Request, StatusCode};
use notes_support::{answering, app, device_credential, notes_route, seed_plugin, send, stub};
use platform_core::config::GatewayConfig;
use platform_persistence::test_support::TestDatabase;

/// The connection that every request names. The stubs do not look it up.
const CONNECTION: &str = "/v1/notes/connections/018f0000-0000-7000-8000-0000000000c1";

/// The largest change batch that notes takes (`MAX_BATCH_BYTES` of its channel routes).
const MAX_BATCH_BYTES: usize = 2 * 1024 * 1024;

/// The largest manifest that notes takes (`MAX_MANIFEST_BYTES` of its channel routes).
const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;

/// The notes route of the shipped example with the default transfer budget, which holds the
/// largest body of every notes route.
fn wide_route(listener: std::net::SocketAddr) -> GatewayConfig {
    let mut gateway = notes_route(listener);
    gateway.budgets.transfer = GatewayConfig::default().budgets.transfer;
    gateway
}

/// Send a body of `maximum` bytes and then one of `maximum + 1` bytes to `path` below the
/// connection, as a paired plugin. The first must reach notes unchanged; the second must be refused
/// at Edge with `413` and must not reach notes.
async fn assert_bounded(method: Method, path: &str, maximum: usize) {
    let (address, mut seen, task) = stub().await;
    let harness = TestDatabase::create().await.expect("a test database");
    let (_, device) = seed_plugin(harness.pool()).await;
    let app = app(&harness, &wide_route(address));
    let credential = device_credential(&app, device).await;
    let request = |length: usize| {
        Request::builder()
            .method(method.clone())
            .uri(format!("{CONNECTION}{path}"))
            .header("authorization", format!("Bearer {credential}"))
            .header("content-length", length)
            .body(Body::from(vec![b'x'; length]))
            .expect("a request")
    };

    let (status, body) = send(&app, request(maximum)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{path}: the largest body that notes takes must reach notes: {body}"
    );
    let forwarded = seen.recv().await.expect("the largest body is forwarded");
    assert_eq!(forwarded.body.len(), maximum, "{path}: the body changed");

    let (status, body) = send(&app, request(maximum + 1)).await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{path}: a body above the largest that notes takes must be refused at Edge: {body}"
    );
    assert_eq!(body["code"], "platform.request.payload_too_large");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), seen.recv())
            .await
            .is_err(),
        "{path}: an oversized body reached the notes service"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn change_batch_admits_the_notes_maximum() {
    assert_bounded(Method::POST, "/changes", MAX_BATCH_BYTES).await;
}

#[tokio::test]
async fn manifest_admits_the_notes_maximum() {
    assert_bounded(Method::POST, "/manifest", MAX_MANIFEST_BYTES).await;
}

#[tokio::test]
async fn chunk_admits_the_contract_maximum() {
    let maximum = usize::try_from(ratatoskr_blob_transfer_contracts::CHUNK_SIZE_MAX_BYTES)
        .expect("the chunk maximum fits in usize");
    assert_bounded(Method::PUT, "/blobs/session-1/chunks/0", maximum).await;
}

#[tokio::test]
async fn change_batch_and_manifest_take_the_transfer_timeout() {
    let (address, task) = answering(
        StatusCode::OK,
        serde_json::json!({"committed_sequence": 0}),
        Duration::from_secs(2),
    )
    .await;
    let mut gateway = notes_route(address);
    gateway.budgets.control.response_timeout_seconds = 1;
    gateway.budgets.transfer.response_timeout_seconds = 10;
    let harness = TestDatabase::create().await.expect("a test database");
    let (_, device) = seed_plugin(harness.pool()).await;
    let app = app(&harness, &gateway);
    let credential = device_credential(&app, device).await;

    for path in ["/changes", "/manifest"] {
        let (status, body) = send(
            &app,
            Request::builder()
                .method("POST")
                .uri(format!("{CONNECTION}{path}"))
                .header("authorization", format!("Bearer {credential}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"changes":[]}"#))
                .expect("a request"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: an answer within the transfer budget must reach the device: {body}"
        );
    }
    task.abort();
    harness.cleanup().await.expect("cleanup");
}
