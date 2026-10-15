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
use notes_support::{
    OWNER_CREDENTIAL, answering, answering_with, app, device_credential, notes_route, seed_owner,
    seed_plugin, send, send_with_headers, stub, values,
};
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

#[tokio::test]
async fn owner_session_is_refused_on_device_routes() {
    let (address, mut seen, task) = stub().await;
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &notes_route(address));
    let request = |method: Method, uri: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .expect("a request")
    };

    for (method, path) in [
        (Method::POST, "/changes"),
        (Method::POST, "/manifest"),
        (Method::POST, "/blobs"),
        (Method::PUT, "/blobs/session-1/chunks/0"),
        (Method::GET, "/blobs/session-1/status"),
        (Method::POST, "/blobs/session-1/finalize"),
    ] {
        let (status, body) = send(&app, request(method, &format!("{CONNECTION}{path}"))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{path}: a device route must refuse a session that is not a device: {body}"
        );
        assert_eq!(body["code"], "platform.auth.forbidden");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), seen.recv())
                .await
                .is_err(),
            "{path}: a request without a device reached the notes service"
        );
    }

    let revision = "a".repeat(64);
    for (method, path, query) in [
        (Method::GET, "/v1/notes/connections".to_owned(), None),
        (Method::GET, format!("{CONNECTION}/files"), None),
        (
            Method::GET,
            format!("{CONNECTION}/files/Daily/2024-01-01.md"),
            Some(format!("revision={revision}")),
        ),
        (
            Method::GET,
            format!("{CONNECTION}/index/Daily/2024-01-01.md"),
            None,
        ),
        (Method::PUT, format!("{CONNECTION}/excluded-folders"), None),
        (Method::DELETE, CONNECTION.to_owned(), None),
    ] {
        let uri = query
            .as_ref()
            .map_or_else(|| path.clone(), |query| format!("{path}?{query}"));
        let (status, body) = send(&app, request(method.clone(), &uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let forwarded = seen.recv().await.expect("the owner request is forwarded");
        assert_eq!(forwarded.method, method);
        assert_eq!(forwarded.path, path);
        assert_eq!(forwarded.query, query);
        assert_eq!(
            values(&forwarded.headers, "x-ratatoskr-user-id"),
            [owner.to_string()]
        );
        assert!(forwarded.headers.get("x-ratatoskr-device-id").is_none());
        assert!(forwarded.headers.get("x-ratatoskr-device-kind").is_none());
    }
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn redirect_from_a_notes_route_is_not_relayed() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    for status in [
        StatusCode::MOVED_PERMANENTLY,
        StatusCode::FOUND,
        StatusCode::SEE_OTHER,
        StatusCode::TEMPORARY_REDIRECT,
        StatusCode::PERMANENT_REDIRECT,
    ] {
        let (address, task) = answering_with(
            status,
            vec![("location", "https://elsewhere.invalid/vault")],
            serde_json::json!({}),
            Duration::ZERO,
        )
        .await;
        let app = app(&harness, &notes_route(address));

        let (answer, headers, body) = send_with_headers(
            &app,
            Request::builder()
                .method("GET")
                .uri(format!("{CONNECTION}/files/Daily/2024-01-01.md"))
                .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await;

        assert_eq!(
            answer,
            StatusCode::BAD_GATEWAY,
            "a {status} from notes must not reach the client: {body}"
        );
        assert_eq!(body["code"], "edge.upstream_invalid_response");
        assert!(
            headers.get("location").is_none(),
            "{status}: Edge relayed the Location"
        );
        task.abort();
    }
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn file_path_segments_reach_notes_unchanged() {
    let (address, mut seen, task) = stub().await;
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let app = app(&harness, &notes_route(address));
    let revision = "b".repeat(64);

    for (path, query) in [
        (format!("{CONNECTION}/files/A%20B/Note%201.md"), None),
        (
            format!("{CONNECTION}/files/A%20B/C%2FD/%2E%2E/Caf%C3%A9%20%25%2B.md"),
            Some(format!("revision={revision}")),
        ),
        (
            format!("{CONNECTION}/files/Deep/er/still/deeper/x.md"),
            None,
        ),
        (format!("{CONNECTION}/index/A%20B/C%2FD/Note%201.md"), None),
    ] {
        let uri = query
            .as_ref()
            .map_or_else(|| path.clone(), |query| format!("{path}?{query}"));
        let (status, body) = send(
            &app,
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let forwarded = seen.recv().await.expect("the read is forwarded");
        assert_eq!(forwarded.path, path, "Edge changed the path of a file read");
        assert_eq!(forwarded.query, query);
    }
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn file_bytes_stream_without_buffering() {
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let held = release.clone();
    let router = axum::Router::new().fallback(move || {
        let held = held.clone();
        async move {
            let first = futures_util::stream::once(async {
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"first"))
            });
            let rest = futures_util::stream::once(async move {
                held.notified().await;
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"rest"))
            });
            Body::from_stream(futures_util::StreamExt::chain(first, rest))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the notes stub serves");
    });
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let app = app(&harness, &notes_route(address));

    let response = tower::ServiceExt::oneshot(
        app,
        Request::builder()
            .method("GET")
            .uri(format!("{CONNECTION}/files/Large.pdf"))
            .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await
    .expect("a response");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let first = tokio::time::timeout(
        Duration::from_secs(5),
        http_body_util::BodyExt::frame(&mut body),
    )
    .await
    .expect("the first bytes arrive while notes holds the rest")
    .expect("a frame")
    .expect("a body frame")
    .into_data()
    .expect("data");
    assert_eq!(first.as_ref(), b"first");

    release.notify_one();
    let rest = http_body_util::BodyExt::collect(body)
        .await
        .expect("the rest of the body")
        .to_bytes();
    assert_eq!(rest.as_ref(), b"rest");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn device_credential_is_refused_on_owner_routes() {
    let (address, mut seen, task) = stub().await;
    let harness = TestDatabase::create().await.expect("a test database");
    let (_, device) = seed_plugin(harness.pool()).await;
    let app = app(&harness, &notes_route(address));
    let credential = device_credential(&app, device).await;
    let request = |method: Method, uri: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"channel":"plugin"}"#))
            .expect("a request")
    };

    let revision = "c".repeat(64);
    for (method, uri) in [
        (Method::GET, "/v1/notes/connections".to_owned()),
        (Method::GET, format!("{CONNECTION}/files")),
        (
            Method::GET,
            format!("{CONNECTION}/files/Daily/2024-01-01.md?revision={revision}"),
        ),
        (
            Method::GET,
            format!("{CONNECTION}/index/Daily/2024-01-01.md"),
        ),
        (Method::PUT, format!("{CONNECTION}/excluded-folders")),
        (Method::DELETE, CONNECTION.to_owned()),
    ] {
        let (status, body) = send(&app, request(method, &uri)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{uri}: an owner route must refuse a device credential: {body}"
        );
        assert_eq!(body["code"], "platform.auth.forbidden");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), seen.recv())
                .await
                .is_err(),
            "{uri}: a device request reached an owner route of notes"
        );
    }

    let (status, body) = send(&app, request(Method::POST, "/v1/notes/connections")).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "a device keeps the connect: {body}"
    );
    let forwarded = seen.recv().await.expect("the connect is forwarded");
    assert_eq!(forwarded.path, "/v1/notes/connections");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}
