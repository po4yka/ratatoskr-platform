//! SITE-027 task 12.3: a site upload uses the media types of the real Notes transfer routes.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod site_owner_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::routing::{post, put};
use axum::{Json, Router};
use http::{HeaderMap, Request, StatusCode};
use platform_persistence::test_support::TestDatabase;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use site_owner_support::{COOKIE, HOSTNAME, OWNER, SITE, Stub, approval, primary, send, sign_in};

const CONNECTION: &str = "018f0000-0000-7000-8000-000000000b01";
const SOURCE: &str = "src_media_types";
const TOKEN: &str = "rst_0v8k4a2j9pm1d7n5tp3es6uabfhij1cm4nop5qrs";
const CONTENT: &str = "# Draft\nmedia type regression\n";
const CAPABILITIES: &[&str] = &["pages.read", "draft.read", "notes.edit"];

type Steps = Arc<AtomicUsize>;

fn digest() -> String {
    format!("{:x}", Sha256::digest(CONTENT.as_bytes()))
}

/// The JSON extractor rejects an absent or incorrect media type before this handler runs.
async fn open(State(steps): State<Steps>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    assert_eq!(
        body.get("declared_size_bytes").and_then(Value::as_u64),
        Some(CONTENT.len() as u64)
    );
    steps.fetch_add(1, Ordering::Relaxed);
    (
        StatusCode::CREATED,
        Json(
            json!({"resumption_token": TOKEN, "chunk_size_bytes": 16_777_216,
                    "expires_at": "2030-01-01T00:00:00Z"}),
        ),
    )
}

async fn chunk(
    State(steps): State<Steps>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/octet-stream")
    {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({})));
    }
    assert!(
        body.as_ref() == CONTENT.as_bytes(),
        "the raw content is unchanged"
    );
    steps.fetch_add(1, Ordering::Relaxed);
    (
        StatusCode::OK,
        Json(json!({"resumption_token": TOKEN, "chunk_index": 0,
                    "received_chunks_count": 1, "idempotent_replay": false})),
    )
}

/// Finalization has its own JSON extraction boundary, after the raw chunk request.
async fn finalize(State(steps): State<Steps>, Json(body): Json<Value>) -> Json<Value> {
    assert!(
        body.get("resumption_token")
            .and_then(Value::as_str)
            .is_some_and(|value| value == TOKEN),
        "finalize names its upload session"
    );
    steps.fetch_add(1, Ordering::Relaxed);
    Json(json!({"outcome": "stored", "blob_ref": {
        "owner_service": "ratatoskr-notes",
        "digest": {"algorithm": "sha256", "hex": digest()},
        "media_type": "text/markdown",
        "length_bytes": CONTENT.len(),
    }}))
}

#[tokio::test]
async fn site_upload_succeeds_when_notes_requires_its_request_media_types() {
    let harness = TestDatabase::create().await.expect("a test database");
    let sites = Stub::start().await;
    sites.manifest(SITE, HOSTNAME, CAPABILITIES);
    sites.answers.lock().expect("answers").insert(
        format!("/internal/sites/{SITE}/sources/{SOURCE}"),
        (
            200,
            json!({"connection_id": CONNECTION, "path": "Garden/Start.md"}),
        ),
    );
    let steps = Arc::new(AtomicUsize::new(0));
    let blobs = "/v1/notes/connections/{connection}/blobs";
    let upstream_status = Arc::new(AtomicU16::new(0));
    let record_status = Arc::clone(&upstream_status);
    let notes = Router::new()
        .route(blobs, post(open))
        .route(&format!("{blobs}/{{token}}/chunks/{{index}}"), put(chunk))
        .route(&format!("{blobs}/{{token}}/finalize"), post(finalize))
        .with_state(Arc::clone(&steps))
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let status = Arc::clone(&record_status);
                async move {
                    let response = next.run(request).await;
                    status.store(response.status().as_u16(), Ordering::Relaxed);
                    response
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a listener");
    let address = listener.local_addr().expect("a listener address");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, notes)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await
            .expect("Notes stub serves");
    });
    let owner = primary(harness.pool(), OWNER, true).await;
    approval(
        harness.pool(),
        owner,
        SITE,
        HOSTNAME,
        CAPABILITIES,
        "approved",
    )
    .await;
    let state =
        site_owner_support::state(&harness, sites.address, &[("notes", "/v1/notes", address)]);
    let app = site_owner_support::app(state);
    let cookie = sign_in(&app, SITE, HOSTNAME).await;
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/sites/{SITE}/drafts/sources/{SOURCE}/blobs"))
        .header("cookie", format!("{COOKIE}={cookie}"))
        .header("x-ratatoskr-site-request", "1")
        .header("origin", format!("https://{HOSTNAME}"))
        .header("content-type", "text/markdown")
        .body(Body::from(CONTENT))
        .expect("a request");
    let (status, _, answer) = send(&app, request).await;
    stop.send(()).expect("stop Notes stub");
    server.await.expect("Notes stub exits");
    harness.cleanup().await.expect("cleanup");
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the public upload status; Notes status {}",
        upstream_status.load(Ordering::Relaxed)
    );
    assert_eq!(
        steps.load(Ordering::Relaxed),
        3,
        "all Notes upload steps completed"
    );
    assert_eq!(
        answer.get("content_hash").and_then(Value::as_str),
        Some(digest().as_str())
    );
}
