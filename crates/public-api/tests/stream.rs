//! The progress stream: replay, follow, ownership, and termination.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use futures_util::StreamExt as _;
use http::{Request, StatusCode};
use platform_core::RuntimeRole;
use platform_core::config::PublicConfig;
use platform_http::HttpState;
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_operation_contracts::OperationStatus;
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "stream-credential";
const AUDIENCE: &str = "edge";
const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

/// An `ApiState` with a healthy database and a configured bus — the two facts
/// `GET /v2/capabilities` reads. Every route exercised here needs both.
fn state(harness: &TestDatabase) -> ApiState {
    let health = Arc::new(platform_http::RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    ApiState::new(harness.database.clone(), AUDIENCE, health, true)
}

fn app(state: ApiState) -> Router {
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 15,
        max_body_bytes: 1_048_576,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(std::sync::Arc::new(state)),
    )
}

async fn seed_user(pool: &sqlx::PgPool, credential: &str) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind: SessionKind::Browser,
            device_id: None,
            audience: AUDIENCE,
            token: Some(auth::credential_digest(credential)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
}

/// Drive an operation through a few statuses, so there is a history to stream.
async fn seed_operation(pool: &sqlx::PgPool, owner: Uuid, statuses: &[OperationStatus]) -> Uuid {
    let operation = platform_operations::accept(
        pool,
        owner,
        "content.capture.submit",
        CORRELATION,
        None,
        now(),
    )
    .await
    .expect("accepting");

    for status in statuses {
        let mut transaction = pool.begin().await.expect("a transaction");
        platform_operations::record_status(
            &mut transaction,
            operation.operation_id,
            *status,
            Some("downloading"),
            Some(25),
            Some("working"),
            now(),
        )
        .await
        .expect("a transition");
        transaction.commit().await.expect("commit");
    }
    operation.operation_id
}

/// Read the whole SSE body. Every stream in these tests reaches a terminal status, so it ends on its
/// own; a test that relied on a timeout would be a test of the timeout.
async fn read_stream(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let mut body = response.into_body().into_data_stream();
    let mut text = String::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.expect("a chunk");
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    (status, text)
}

fn stream_request(
    credential: &str,
    operation_id: Uuid,
    last_event_id: Option<&str>,
) -> Request<Body> {
    let mut request = Request::builder()
        .uri(format!("/v2/operations/{operation_id}/events"))
        .header("authorization", format!("Bearer {credential}"));
    if let Some(id) = last_event_id {
        request = request.header("last-event-id", id);
    }
    request.body(Body::empty()).expect("a request")
}

/// S-1. The stream replays the stored history and ends when the operation reaches a terminal status.
#[tokio::test]
async fn the_stream_replays_history_and_ends_at_a_terminal_status() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_user(pool, CREDENTIAL).await;
    let operation_id = seed_operation(
        pool,
        owner,
        &[
            OperationStatus::Queued,
            OperationStatus::Running,
            OperationStatus::Succeeded,
        ],
    )
    .await;
    let app = app(state(&harness));

    let (status, body) = read_stream(&app, stream_request(CREDENTIAL, operation_id, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.matches("event: progress").count(),
        3,
        "every recorded entry must be replayed\n{body}"
    );
    assert!(body.contains(r#""status":"queued""#), "{body}");
    assert!(body.contains(r#""status":"succeeded""#), "{body}");
    assert!(
        body.contains("id: "),
        "every event carries an id so a client can resume\n{body}"
    );

    harness.cleanup().await.expect("cleanup");
}

/// S-2. `Last-Event-ID` resumes after the entry the client already saw, and repeats nothing.
#[tokio::test]
async fn a_reconnect_resumes_after_the_last_event_the_client_saw() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_user(pool, CREDENTIAL).await;
    let operation_id = seed_operation(
        pool,
        owner,
        &[
            OperationStatus::Queued,
            OperationStatus::Running,
            OperationStatus::Succeeded,
        ],
    )
    .await;
    let app = app(state(&harness));

    let entries = platform_operations::progress_since(pool, operation_id, None, 100)
        .await
        .expect("reading progress");
    assert_eq!(entries.len(), 3);
    let first_id = entries[0].progress_id.to_string();

    let (status, body) = read_stream(
        &app,
        stream_request(CREDENTIAL, operation_id, Some(&first_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.matches("event: progress").count(),
        2,
        "a resumed stream must not repeat what the client already has\n{body}"
    );
    assert!(
        !body.contains(&first_id),
        "the entry named by Last-Event-ID must not be sent again\n{body}"
    );

    harness.cleanup().await.expect("cleanup");
}

/// S-3. The stream obeys the same ownership rule as the polling route, with the same refusal.
#[tokio::test]
async fn the_stream_is_readable_only_by_the_owner() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_user(pool, CREDENTIAL).await;
    seed_user(pool, "another-credential").await;
    let operation_id = seed_operation(pool, owner, &[OperationStatus::Succeeded]).await;
    let app = app(state(&harness));

    let (status, body) = read_stream(
        &app,
        stream_request("another-credential", operation_id, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("platform.resource.not_found"), "{body}");

    let (missing, _) = read_stream(&app, stream_request(CREDENTIAL, Uuid::now_v7(), None)).await;
    assert_eq!(
        missing,
        StatusCode::NOT_FOUND,
        "a nonexistent operation and someone else's must be indistinguishable"
    );

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v2/operations/{operation_id}/events"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("a response");
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    harness.cleanup().await.expect("cleanup");
}

/// S-4. The stream carries no bus detail. `ARCHITECTURE.md` S5.5: the event bus is not exposed
/// directly to clients, and S15 forbids an internal subject reaching one.
#[tokio::test]
async fn the_stream_exposes_no_bus_detail() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_user(pool, CREDENTIAL).await;
    let operation_id = seed_operation(pool, owner, &[OperationStatus::Succeeded]).await;
    let app = app(state(&harness));

    let (_, body) = read_stream(&app, stream_request(CREDENTIAL, operation_id, None)).await;
    for leak in [
        "cmd.",
        "evt.",
        "nats",
        "jetstream",
        "outbox",
        "inbox",
        "subject",
    ] {
        assert!(
            !body.to_lowercase().contains(leak),
            "the stream must not carry {leak}\n{body}"
        );
    }

    harness.cleanup().await.expect("cleanup");
}
