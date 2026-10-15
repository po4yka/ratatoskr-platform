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

mod notes_support;

use std::time::Duration;

use axum::body::{Body, Bytes};
use http::{Request, StatusCode};
use notes_support::{
    DEVICE_SECRET, OWNER_CREDENTIAL, answering, app, assert_connect_fails, device_credential,
    notes_route, now, owner_connect, seed_owner, seed_plugin, send, stub, values,
};
use platform_identity::{DeviceKind, SecretDigest};
use platform_persistence::test_support::TestDatabase;
use tokio::net::TcpListener;
use uuid::Uuid;

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

/// Workspace OBS-024 task 10.3: the owner's primary session uploads the content of a write. Edge
/// forwards it with a claim that names the owner and the session kind, and with no device claim.
#[tokio::test]
async fn session_upload_claim_names_the_session_kind() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let forged = Uuid::now_v7().to_string();

    let (status, body) = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/v1/notes/connections/018f0000-0000-7000-8000-0000000000c1/blobs")
            .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
            .header("content-type", "application/json")
            .header("x-ratatoskr-device-id", &forged)
            .header("x-ratatoskr-device-kind", "obsidian_plugin")
            .header("x-ratatoskr-session-kind", "device")
            .body(Body::from(r#"{"declared_size_bytes":7}"#))
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let request = seen.recv().await.expect("one upstream request");
    assert_eq!(
        request.path,
        "/v1/notes/connections/018f0000-0000-7000-8000-0000000000c1/blobs"
    );
    assert_eq!(
        values(&request.headers, "x-ratatoskr-user-id"),
        [owner.to_string()]
    );
    assert_eq!(
        values(&request.headers, "x-ratatoskr-session-kind"),
        ["browser"]
    );
    assert!(values(&request.headers, "x-ratatoskr-device-id").is_empty());
    assert!(values(&request.headers, "x-ratatoskr-device-kind").is_empty());
    assert!(request.headers.get("authorization").is_none());
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn blob_upload_uses_the_transfer_budget() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let (_, device) = seed_plugin(harness.pool()).await;
    let app = app(&harness, &gateway);
    let credential = device_credential(&app, device).await;
    let bytes = vec![b'x'; 2048];

    let (status, body) = send(
        &app,
        Request::builder()
            .method("PUT")
            .uri("/v1/notes/connections/018f0000-0000-7000-8000-0000000000c1/blobs/session-1/chunks/0")
            .header("authorization", format!("Bearer {credential}"))
            .header("content-length", bytes.len())
            .body(Body::from(bytes.clone()))
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let request = seen.recv().await.expect("the blob request");
    assert_eq!(request.body.len(), bytes.len());
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn unreachable_notes_fails_the_connect_operation() {
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = closed.local_addr().expect("a listener address");
    drop(closed);

    assert_connect_fails(
        &notes_route(address),
        owner_connect(),
        StatusCode::SERVICE_UNAVAILABLE,
        "edge.upstream_unavailable",
    )
    .await;
}

#[tokio::test]
async fn timed_out_notes_fails_the_connect_operation() {
    let (address, task) = answering(
        StatusCode::ACCEPTED,
        serde_json::json!({}),
        Duration::from_secs(5),
    )
    .await;
    let mut gateway = notes_route(address);
    gateway.budgets.control.response_timeout_seconds = 1;

    assert_connect_fails(
        &gateway,
        owner_connect(),
        StatusCode::GATEWAY_TIMEOUT,
        "edge.upstream_timeout",
    )
    .await;
    task.abort();
}

#[tokio::test]
async fn refused_connect_fails_the_operation_with_the_notes_code() {
    let (address, task) = answering(
        StatusCode::CONFLICT,
        serde_json::json!({
            "code": "notes.connection.vault_already_connected",
            "message": "This vault is already connected.",
            "retryable": false,
        }),
        Duration::ZERO,
    )
    .await;

    assert_connect_fails(
        &notes_route(address),
        owner_connect(),
        StatusCode::CONFLICT,
        "notes.connection.vault_already_connected",
    )
    .await;
    task.abort();
}

#[tokio::test]
async fn oversized_streamed_connect_is_refused_with_413() {
    let (address, task) = answering(
        StatusCode::ACCEPTED,
        serde_json::json!({}),
        Duration::from_secs(2),
    )
    .await;
    // No length is declared, so the body budget can stop it only while it streams.
    let chunks = (0..2).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 800])));
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
        StatusCode::PAYLOAD_TOO_LARGE,
        "platform.request.payload_too_large",
    )
    .await;
    task.abort();
}

#[tokio::test]
async fn connect_with_a_trailing_slash_is_refused_at_edge() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let request = |method: &str| {
        Request::builder()
            .method(method)
            .uri("/v1/notes/connections/")
            .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"channel":"sync"}"#))
            .expect("a request")
    };

    let (status, body) = send(&app, request("POST")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), seen.recv())
            .await
            .is_err(),
        "a connect with a trailing slash reached the notes service"
    );

    let (status, body) = send(&app, request("GET")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let forwarded = seen.recv().await.expect("the read is proxied");
    assert_eq!(forwarded.path, "/v1/notes/connections/");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}
