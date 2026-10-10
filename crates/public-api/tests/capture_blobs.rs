//! Typed capture commands and the Telegram blob route (XR-021 CONTRACTS.md S11).

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use axum::body::Body;
use http::{Request, StatusCode};
use platform_identity::SessionKind;
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

mod common_session;

use common_session::{AUDIENCE, CREDENTIAL, app, seed_session, send, state};

const CAPTURE: &str = r#"{"url":"https://example.test/article"}"#;

fn submit(credential: Option<&str>, key: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/captures")
        .header("content-type", "application/json");
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

/// The outbox row of `operation_id`: subject and payload.
async fn outbox_row(pool: &sqlx::PgPool, operation_id: Uuid) -> (String, serde_json::Value) {
    sqlx::query_as("select subject, payload from operations.outbox where operation_id = $1")
        .bind(operation_id)
        .fetch_one(pool)
        .await
        .expect("the command row")
}

fn operation_of(body: &serde_json::Value) -> Uuid {
    body["operation_id"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .expect("an operation id")
}

/// S11. `content.capture.requested.v1` is a typed `CommandEnvelope` with a `ContentCaptureRequested`
/// payload, not the legacy command document the extractor used to hand-decode.
#[tokio::test]
async fn capture_outbox_row_decodes_as_content_capture_requested() {
    use ratatoskr_document_contracts::ContentCaptureRequested;
    use ratatoskr_event_envelope::CommandEnvelope;

    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed_session(pool, CREDENTIAL, AUDIENCE, SessionKind::Browser).await;
    let app = app(state(&harness));

    let (status, body) = send(&app, submit(Some(CREDENTIAL), Some("typed-1"), CAPTURE)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let operation_id = operation_of(&body);
    let (subject, stored) = outbox_row(pool, operation_id).await;
    assert_eq!(subject, "cmd.content.capture.requested.v1");

    let envelope = CommandEnvelope::from_json(stored.to_string().as_bytes())
        .expect("the stored row is a contract CommandEnvelope");
    assert_eq!(stored["producer"], "ratatoskr-platform");
    assert_eq!(stored["aggregate_id"], format!("operation:{operation_id}"));
    assert_eq!(stored["tenant_id"], format!("user:{user}"));
    assert!(
        stored["correlation_id"]
            .as_str()
            .is_some_and(|value| value.starts_with("correlation:")),
        "the request correlation is carried unchanged: {stored}"
    );
    let payload: ContentCaptureRequested = envelope
        .payload_as()
        .expect("the payload is a ContentCaptureRequested");
    payload.validate().expect("exactly one source");
    assert_eq!(payload.operation_id.0, operation_id);
    assert_eq!(
        payload.url.as_ref().map(ToString::to_string).as_deref(),
        Some("https://example.test/article")
    );
    assert!(payload.blob.is_none());
    let key_digest = ring::digest::digest(&ring::digest::SHA256, b"typed-1");
    let expected = key_digest
        .as_ref()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            use core::fmt::Write as _;
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
            hex
        });
    assert_eq!(
        stored["payload"]["idempotency_key"]["hex"], expected,
        "the idempotency key is the sha256 of the caller's string"
    );
    harness.cleanup().await.expect("cleanup");
}

const MIB: u64 = 1_048_576;

fn blob_body(owner: &str, length: u64) -> String {
    format!(
        r#"{{"blob":{{"owner_service":"{owner}","digest":{{"algorithm":"sha256","hex":"{hex}"}},"media_type":"application/pdf","length_bytes":{length}}}}}"#,
        hex = "ab".repeat(32)
    )
}

fn submit_blob(credential: Option<&str>, key: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/captures/blobs")
        .header("content-type", "application/json");
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

async fn outbox_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from operations.outbox")
        .fetch_one(pool)
        .await
        .expect("counting commands")
}

async fn operation_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from operations.operations")
        .fetch_one(pool)
        .await
        .expect("counting operations")
}

/// S11. A Telegram Mini App session submits the PDF it stored, by reference, and exactly one command
/// carrying that reference reaches the outbox.
#[tokio::test]
async fn a_telegram_blob_capture_is_accepted_and_emits_one_blob_command() {
    use ratatoskr_document_contracts::ContentCaptureRequested;
    use ratatoskr_event_envelope::CommandEnvelope;

    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed_session(pool, CREDENTIAL, AUDIENCE, SessionKind::TelegramMiniApp).await;
    let app = app(state(&harness));

    let body = blob_body("ratatoskr-telegram", 13_264);
    let (status, accepted) = send(&app, submit_blob(Some(CREDENTIAL), Some("blob-1"), &body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["status"], "accepted");
    let operation_id = operation_of(&accepted);

    assert_eq!(outbox_rows(pool).await, 1);
    let (subject, stored) = outbox_row(pool, operation_id).await;
    assert_eq!(subject, "cmd.content.capture.requested.v1");
    assert_eq!(stored["producer"], "ratatoskr-platform");
    assert_eq!(stored["tenant_id"], format!("user:{user}"));
    assert_eq!(stored["aggregate_id"], format!("operation:{operation_id}"));
    let envelope = CommandEnvelope::from_json(stored.to_string().as_bytes()).expect("an envelope");
    let payload: ContentCaptureRequested = envelope.payload_as().expect("the blob payload");
    assert!(payload.url.is_none());
    let requested: serde_json::Value = serde_json::from_str(&body).expect("the request body");
    assert_eq!(
        serde_json::to_value(payload.blob.expect("a blob")).expect("a blob value"),
        requested["blob"],
        "the command carries the blob exactly as the client submitted it"
    );
    let operation = platform_operations::find(pool, operation_id)
        .await
        .expect("reading")
        .expect("the operation");
    assert_eq!(operation.kind, "content.capture.submit");
    assert_eq!(operation.owner_user_id, user);
    harness.cleanup().await.expect("cleanup");
}

/// S11. The blob route exists for the Telegram bot. Any other kind of session is refused before
/// anything is written: the store is content-addressed and shared, so the session kind is the gate.
#[tokio::test]
async fn a_blob_capture_from_a_non_telegram_session_is_forbidden() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let mut apps = Vec::new();
    for (index, kind) in [
        SessionKind::Browser,
        SessionKind::Device,
        SessionKind::ApiToken,
    ]
    .into_iter()
    .enumerate()
    {
        let credential = format!("non-telegram-credential-{index}-0000000");
        seed_session(pool, &credential, AUDIENCE, kind).await;
        apps.push(credential);
    }
    let app = app(state(&harness));

    for credential in apps {
        let (status, body) = send(
            &app,
            submit_blob(
                Some(&credential),
                Some("blob-forbidden"),
                &blob_body("ratatoskr-telegram", 100),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "platform.auth.forbidden");
    }
    assert_eq!(outbox_rows(pool).await, 0);
    assert_eq!(operation_rows(pool).await, 0);
    harness.cleanup().await.expect("cleanup");
}

/// S11. Even a Telegram session may only name bytes the Telegram service owns.
#[tokio::test]
async fn a_blob_capture_naming_a_foreign_owner_is_forbidden() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed_session(pool, CREDENTIAL, AUDIENCE, SessionKind::TelegramMiniApp).await;
    let app = app(state(&harness));

    let (status, body) = send(
        &app,
        submit_blob(
            Some(CREDENTIAL),
            Some("blob-foreign"),
            &blob_body("ratatoskr-vault", 100),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "platform.auth.forbidden");
    assert_eq!(outbox_rows(pool).await, 0);
    assert_eq!(operation_rows(pool).await, 0);
    harness.cleanup().await.expect("cleanup");
}

/// S11. A blob has between one byte and the extractor's 50 MiB PDF ceiling.
#[tokio::test]
async fn a_blob_capture_with_zero_or_oversize_length_is_invalid() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed_session(pool, CREDENTIAL, AUDIENCE, SessionKind::TelegramMiniApp).await;
    let app = app(state(&harness));

    for (key, length) in [("blob-zero", 0), ("blob-over", 50 * MIB + 1)] {
        let (status, body) = send(
            &app,
            submit_blob(
                Some(CREDENTIAL),
                Some(key),
                &blob_body("ratatoskr-telegram", length),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{length}: {body}");
        assert_eq!(body["code"], "platform.request.invalid");
    }
    let (status, body) = send(
        &app,
        submit_blob(
            Some(CREDENTIAL),
            Some("blob-at-ceiling"),
            &blob_body("ratatoskr-telegram", 50 * MIB),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(outbox_rows(pool).await, 1, "only the valid one is written");
    harness.cleanup().await.expect("cleanup");
}

/// S11. A retry with the same key and body returns the original operation and writes nothing.
#[tokio::test]
async fn a_blob_capture_retry_returns_the_original_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed_session(pool, CREDENTIAL, AUDIENCE, SessionKind::TelegramMiniApp).await;
    let app = app(state(&harness));
    let body = blob_body("ratatoskr-telegram", 4096);

    let (first_status, first) = send(
        &app,
        submit_blob(Some(CREDENTIAL), Some("blob-retry"), &body),
    )
    .await;
    let (second_status, second) = send(
        &app,
        submit_blob(Some(CREDENTIAL), Some("blob-retry"), &body),
    )
    .await;
    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(second_status, StatusCode::ACCEPTED);
    assert_eq!(first["operation_id"], second["operation_id"]);
    assert_eq!(operation_rows(pool).await, 1);
    assert_eq!(outbox_rows(pool).await, 1);
    harness.cleanup().await.expect("cleanup");
}

/// S11. The same key with another blob is refused, because honouring it would silently replace what
/// the first request meant.
#[tokio::test]
async fn the_same_key_with_a_different_blob_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed_session(pool, CREDENTIAL, AUDIENCE, SessionKind::TelegramMiniApp).await;
    let app = app(state(&harness));

    send(
        &app,
        submit_blob(
            Some(CREDENTIAL),
            Some("blob-key"),
            &blob_body("ratatoskr-telegram", 4096),
        ),
    )
    .await;
    let (status, body) = send(
        &app,
        submit_blob(
            Some(CREDENTIAL),
            Some("blob-key"),
            &blob_body("ratatoskr-telegram", 8192),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_conflict");
    harness.cleanup().await.expect("cleanup");
}

/// S11. As on the URL route, a replayable mutation without a key is a client error.
#[tokio::test]
async fn a_blob_capture_without_an_idempotency_key_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed_session(pool, CREDENTIAL, AUDIENCE, SessionKind::TelegramMiniApp).await;
    let app = app(state(&harness));

    let (status, body) = send(
        &app,
        submit_blob(
            Some(CREDENTIAL),
            None,
            &blob_body("ratatoskr-telegram", 4096),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_key_required");
    assert_eq!(outbox_rows(pool).await, 0);
    harness.cleanup().await.expect("cleanup");
}
