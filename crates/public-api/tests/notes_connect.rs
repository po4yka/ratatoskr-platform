//! The connect call of `ratatoskr-notes` at Edge when the connect does not complete (workspace
//! change `add-notes-vault-connections`, tasks 9.14 and 9.15), and the streamed body budget of the
//! generic proxy.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod notes_support;

use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use http::{Request, StatusCode};
use notes_support::{
    CONNECT_FAILED, OWNER_CREDENTIAL, answering, answering_with, app, assert_connect_fails,
    assert_failed_with, connect_operation, device_credential, notes_route, seed_owner, seed_plugin,
    send, send_with_headers,
};
use platform_persistence::test_support::TestDatabase;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

/// The text of a notes error envelope that must not reach the operation.
const NOTES_TEXT: &str = "MARKER-NOTES-TEXT-5e1a: vault Private of owner@example.invalid";

/// The `409` error envelope of the notes service, with [`NOTES_TEXT`] as its message.
fn vault_already_connected() -> serde_json::Value {
    serde_json::json!({
        "code": "notes.connection.vault_already_connected",
        "message": NOTES_TEXT,
        "retryable": false,
    })
}

/// A notes stub that reports each request when it has read the body, and answers `409` after
/// `delay`.
async fn refusing_after(
    delay: Duration,
) -> (
    std::net::SocketAddr,
    mpsc::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let (arrived_tx, arrived_rx) = mpsc::channel::<()>(1);
    let router = Router::new().fallback(move |request: axum::extract::Request| {
        let arrived_tx = arrived_tx.clone();
        async move {
            let _ = axum::body::to_bytes(request.into_body(), usize::MAX).await;
            let _ = arrived_tx.send(()).await;
            tokio::time::sleep(delay).await;
            (StatusCode::CONFLICT, axum::Json(vault_already_connected()))
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
    (address, arrived_rx, task)
}

#[tokio::test]
async fn client_disconnect_does_not_stop_the_terminal_write() {
    let (address, mut arrived, task) = refusing_after(Duration::from_millis(300)).await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);

    // A client that disconnects drops the handler future, as the server does on a closed
    // connection.
    let client = tokio::spawn(app.oneshot(notes_support::owner_connect()));
    tokio::time::timeout(Duration::from_secs(5), arrived.recv())
        .await
        .expect("the connect reaches notes")
        .expect("one connect");
    client.abort();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while connect_operation(harness.pool(), owner).await.status
        != ratatoskr_operation_contracts::OperationStatus::Failed
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_failed_with(
        harness.pool(),
        owner,
        "notes.connection.vault_already_connected",
    )
    .await;
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn aborted_connect_body_is_a_request_error() {
    let (address, task) = answering(
        StatusCode::ACCEPTED,
        serde_json::json!({}),
        Duration::from_secs(2),
    )
    .await;
    // The client sends a part of the body and then stops, as an aborted upload does.
    let chunks = [
        Ok(Bytes::from_static(br#"{"channel":"#)),
        Err(std::io::Error::other("the client aborted the upload")),
    ];
    let request = Request::builder()
        .method("POST")
        .uri("/v1/notes/connections")
        .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
        .header("content-type", "application/json")
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .expect("a request");

    assert_connect_fails(
        &notes_route(address),
        request,
        StatusCode::BAD_REQUEST,
        "platform.request.invalid",
    )
    .await;
    task.abort();
}

#[tokio::test]
async fn redirect_from_notes_is_an_invalid_response() {
    let (address, task) = answering_with(
        StatusCode::TEMPORARY_REDIRECT,
        vec![("location", "https://elsewhere.invalid/v1/notes/connections")],
        serde_json::json!({}),
        Duration::ZERO,
    )
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &notes_route(address));

    let (status, headers, body) = send_with_headers(&app, notes_support::owner_connect()).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "edge.upstream_invalid_response");
    assert!(
        headers.get("location").is_none(),
        "Edge relayed the redirect of notes"
    );
    assert_failed_with(harness.pool(), owner, "edge.upstream_invalid_response").await;
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn success_other_than_202_is_an_invalid_response() {
    let (address, task) = answering(
        StatusCode::OK,
        serde_json::json!({
            "connection_id": "018f0000-0000-7000-8000-0000000000c1",
            "channel": "sync",
            "state": "connecting",
        }),
        Duration::ZERO,
    )
    .await;

    assert_connect_fails(
        &notes_route(address),
        notes_support::owner_connect(),
        StatusCode::BAD_GATEWAY,
        "edge.upstream_invalid_response",
    )
    .await;
    task.abort();
}

#[tokio::test]
async fn refused_connect_keeps_no_notes_text() {
    let (address, task) = answering(
        StatusCode::CONFLICT,
        vault_already_connected(),
        Duration::ZERO,
    )
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &notes_route(address));

    let (status, body) = send(&app, notes_support::owner_connect()).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["message"], NOTES_TEXT, "Edge relays the notes refusal");
    assert_failed_with(
        harness.pool(),
        owner,
        "notes.connection.vault_already_connected",
    )
    .await;
    let operation = connect_operation(harness.pool(), owner).await;
    let stored = serde_json::to_string(&operation).expect("the snapshot as JSON");
    assert!(
        !stored.contains("MARKER-NOTES-TEXT"),
        "the operation holds the notes text: {stored}"
    );
    for table in ["operation_errors", "operation_progress"] {
        let (found,): (i64,) = sqlx::query_as(&format!(
            "select count(*) from operations.{table} as row \
              where row::text like '%MARKER-NOTES-TEXT%'"
        ))
        .fetch_one(harness.pool())
        .await
        .expect("the table scan");
        assert_eq!(found, 0, "operations.{table} holds the notes text");
    }
    let messages: Vec<String> = sqlx::query_scalar(
        "select message from operations.operation_progress where status = 'failed'",
    )
    .fetch_all(harness.pool())
    .await
    .expect("the progress messages");
    assert_eq!(messages, [CONNECT_FAILED]);
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn oversized_streamed_blob_upload_is_refused_with_413() {
    let (address, task) = answering(
        StatusCode::OK,
        serde_json::json!({}),
        Duration::from_secs(2),
    )
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    let (_, device) = seed_plugin(harness.pool()).await;
    let app = app(&harness, &notes_route(address));
    let credential = device_credential(&app, device).await;
    // No length is declared, so the transfer budget can stop the upload only while it streams.
    let chunks = (0..3).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 524_288])));
    let request = Request::builder()
        .method("PUT")
        .uri("/v1/notes/connections/018f0000-0000-7000-8000-0000000000c1/blobs/session-1/chunks/0")
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .expect("a request");

    let (status, body) = send(&app, request).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "platform.request.payload_too_large");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// A notes stub that answers `202` and then never ends the body of the answer.
async fn stalling() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(|| async {
        let chunks = futures_util::StreamExt::chain(
            futures_util::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"{")) }),
            futures_util::stream::pending(),
        );
        (StatusCode::ACCEPTED, Body::from_stream(chunks))
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

#[tokio::test]
async fn stalled_connect_answer_fails_within_the_budget() {
    let (address, task) = stalling().await;
    let mut gateway = notes_route(address);
    gateway.budgets.control.response_timeout_seconds = 1;
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);

    let answered = tokio::time::timeout(
        Duration::from_secs(5),
        send(&app, notes_support::owner_connect()),
    )
    .await;

    let (status, body) = answered.unwrap_or_else(|_| {
        panic!("a connect whose answer stalls must end within the control budget")
    });
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_failed_with(harness.pool(), owner, "edge.upstream_timeout").await;
    task.abort();
    harness.cleanup().await.expect("cleanup");
}
