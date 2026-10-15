//! The upload of a site edit when notes refuses a step (workspace change
//! `add-site-owner-editing-and-live`, review L9 of SITE-027): Edge closes the notes upload
//! session with `DELETE /v1/notes/connections/{id}/blobs/{token}`, so a failed upload does not
//! hold one of the connection's sessions until it expires.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in a test binary"
)]

mod site_owner_support;

use axum::body::Body;
use http::Request;
use platform_persistence::test_support::TestDatabase;
use serde_json::json;
use site_owner_support::{COOKIE, HOSTNAME, OWNER, SITE, Stub, approval, primary, send, sign_in};

const CAPABILITIES: &[&str] = &["pages.read", "draft.read", "notes.edit"];
const CONNECTION: &str = "018f0000-0000-7000-8000-000000000b01";
const SOURCE: &str = "src_garden_start";
const TOKEN: &str = "rst_0v8k4a2j9pm1d7n5tp3es6uabfhij1cm4nop5qrs";

/// The status of an upload to `site-a` while notes answers `failing` with 500, and the requests
/// that notes received.
async fn upload_while_failing(failing: &str) -> (u16, Vec<(String, String)>) {
    let harness = TestDatabase::create().await.expect("a test database");
    let sites = Stub::start().await;
    let notes = Stub::start().await;
    sites.manifest(SITE, HOSTNAME, CAPABILITIES);
    sites.answers.lock().unwrap().insert(
        format!("/internal/sites/site-a/sources/{SOURCE}"),
        (
            200,
            json!({"connection_id": CONNECTION, "path": "Garden/Start.md"}),
        ),
    );
    let blobs = format!("/v1/notes/connections/{CONNECTION}/blobs");
    {
        let mut answers = notes.answers.lock().unwrap();
        answers.insert(
            blobs.clone(),
            (
                201,
                json!({"resumption_token": TOKEN, "chunk_size_bytes": 16_777_216,
                       "expires_at": "2030-01-01T00:00:00Z"}),
            ),
        );
        answers.insert(
            format!("{blobs}/{TOKEN}/finalize"),
            (200, json!({"outcome": "stored", "blob_ref": {}})),
        );
        answers.insert(
            format!("{blobs}/{TOKEN}/{failing}"),
            (
                500,
                json!({"code": "notes.service.unavailable", "message": "No.", "retryable": true}),
            ),
        );
    }
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
    let state = site_owner_support::state(
        &harness,
        sites.address,
        &[("notes", "/v1/notes", notes.address)],
    );
    let app = site_owner_support::app(state);
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/sites/site-a/drafts/sources/{SOURCE}/blobs"))
        .header("cookie", format!("{COOKIE}={cookie}"))
        .header("x-ratatoskr-site-request", "1")
        .header("origin", format!("https://{HOSTNAME}"))
        .header("content-type", "text/markdown")
        .body(Body::from("# Start\nnew text\n"))
        .expect("a request");
    let (status, _, _) = send(&app, request).await;
    let received = notes
        .received()
        .into_iter()
        .map(|request| (request.method, request.uri))
        .collect();
    harness.cleanup().await.expect("cleanup");
    (status.as_u16(), received)
}

fn closed(received: &[(String, String)]) -> bool {
    let session = format!("/v1/notes/connections/{CONNECTION}/blobs/{TOKEN}");
    received
        .iter()
        .any(|(method, uri)| method == "DELETE" && *uri == session)
}

/// Review L9 of SITE-027: a refused chunk closes the upload session.
#[tokio::test]
async fn failed_chunk_closes_the_upload_session() {
    let (status, received) = upload_while_failing("chunks/0").await;
    assert_eq!(status, 502, "the upload of a refused chunk");
    assert!(closed(&received), "the session stays open: {received:?}");
}

/// Review L9 of SITE-027: a refused finalize closes the upload session.
#[tokio::test]
async fn failed_finalize_closes_the_upload_session() {
    let (status, received) = upload_while_failing("finalize").await;
    assert_eq!(status, 502, "the upload of a refused finalize");
    assert!(closed(&received), "the session stays open: {received:?}");
}
