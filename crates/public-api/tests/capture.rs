//! The public API end to end: authentication, idempotency, the transactional write, and the read.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::PublicConfig;
use platform_http::HttpState;
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "a-test-session-credential";
const AUDIENCE: &str = "edge";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

/// An `ApiState` with a healthy database and a configured bus.
///
/// The two facts `GET /v1/capabilities` reads. Every test here exercises a route that needs both,
/// so the default is "the deployment is whole"; the capability tests are where the other
/// combinations live.
fn state(harness: &TestDatabase) -> ApiState {
    let health = Arc::new(platform_http::RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    ApiState::new(harness.database.clone(), AUDIENCE, health, true)
}

/// The real public pipeline, not a bare router: the middleware is what renders an authored failure
/// into an `ErrorEnvelope`, so a test without it would assert statuses and prove nothing about
/// bodies.
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

/// A user with a live session that authenticates with `credential`.
async fn seed(pool: &sqlx::PgPool, credential: &str, audience: &str) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind: SessionKind::Browser,
            device_id: None,
            audience,
            token: Some(auth::credential_digest(credential)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
}

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

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

const CAPTURE: &str = r#"{"url":"https://example.test/article"}"#;

const X_BROWSER_CAPTURE: &str = r#"{
    "url":"https://x.com/ratatoskr/status/1234567890123456789",
    "social":{
        "provider":"x",
        "captured_at":"2026-08-27T10:00:00Z",
        "acquisition":"browser_extension",
        "saved_authority":"explicit_user_capture"
    }
}"#;

/// C-1. The happy path writes the reservation, the operation and the command in one transaction, and
/// answers with something to poll rather than a result.
#[tokio::test]
async fn a_capture_is_accepted_and_produces_exactly_one_command() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let (status, body) = send(&app, submit(Some(CREDENTIAL), Some("key-1"), CAPTURE)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "accepted");
    let operation_id: Uuid = body["operation_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("an operation id");

    let operation = platform_operations::find(pool, operation_id)
        .await
        .expect("reading")
        .expect("the operation");
    assert_eq!(operation.owner_user_id, user);
    assert_eq!(operation.kind, "content.capture.submit");

    // Exactly one command, addressed to the extractor, carrying this operation.
    let (count, subject): (i64, String) = sqlx::query_as(
        "select count(*), min(subject) from operations.outbox where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .expect("counting commands");
    assert_eq!(count, 1);
    assert_eq!(subject, "cmd.content.capture.requested.v1");

    harness.cleanup().await.expect("cleanup");
}

/// C-1a. An explicit social browser capture is routed to its social owner with provenance, rather
/// than being misrepresented as a generic article-extraction command.
#[tokio::test]
async fn an_explicit_x_browser_capture_produces_a_social_command_with_provenance() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let (status, body) = send(
        &app,
        submit(
            Some(CREDENTIAL),
            Some("x-browser-capture"),
            X_BROWSER_CAPTURE,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let operation_id: Uuid = body["operation_id"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .expect("an operation id");

    let (subject, payload): (String, serde_json::Value) =
        sqlx::query_as("select subject, payload from operations.outbox where operation_id = $1")
            .bind(operation_id)
            .fetch_one(pool)
            .await
            .expect("the social command");
    assert_eq!(subject, "cmd.x.capture.requested.v1");
    assert_eq!(payload["command_type"], "social.capture.requested.v1");
    assert_eq!(
        payload["payload"]["original_permalink"],
        "https://x.com/ratatoskr/status/1234567890123456789"
    );
    assert_eq!(payload["payload"]["provider"], "x");
    assert_eq!(payload["payload"]["acquisition"], "browser_extension");
    assert_eq!(
        payload["payload"]["saved_authority"],
        "explicit_user_capture"
    );

    harness.cleanup().await.expect("cleanup");
}

/// C-2. A retry with the same key and body returns the ORIGINAL operation and creates nothing.
#[tokio::test]
async fn a_retry_returns_the_original_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let (first_status, first) = send(&app, submit(Some(CREDENTIAL), Some("k"), CAPTURE)).await;
    let (second_status, second) = send(&app, submit(Some(CREDENTIAL), Some("k"), CAPTURE)).await;

    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(second_status, StatusCode::ACCEPTED);
    assert_eq!(
        first["operation_id"], second["operation_id"],
        "a retry must return the original operation"
    );

    let operations: i64 = sqlx::query_scalar("select count(*) from operations.operations")
        .fetch_one(pool)
        .await
        .expect("counting");
    let commands: i64 = sqlx::query_scalar("select count(*) from operations.outbox")
        .fetch_one(pool)
        .await
        .expect("counting");
    assert_eq!(operations, 1, "a retry must not create a second operation");
    assert_eq!(commands, 1, "a retry must not emit a second command");

    harness.cleanup().await.expect("cleanup");
}

/// C-3. The same key with a different body is refused, with an envelope the middleware rendered.
#[tokio::test]
async fn the_same_key_with_a_different_body_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    send(&app, submit(Some(CREDENTIAL), Some("k"), CAPTURE)).await;
    let (status, body) = send(
        &app,
        submit(
            Some(CREDENTIAL),
            Some("k"),
            r#"{"url":"https://example.test/other"}"#,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "platform.request.idempotency_conflict");
    assert_eq!(body["retryable"], false);
    assert!(
        body["correlation_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("correlation:")),
        "every failure carries the request's correlation"
    );

    harness.cleanup().await.expect("cleanup");
}

/// C-4. Every way of failing to authenticate looks the same from outside.
#[tokio::test]
async fn authentication_failures_are_indistinguishable() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool, CREDENTIAL, AUDIENCE).await;
    // A session for a different audience: a token minted for another surface must not work here.
    seed(pool, "other-surface-credential", "mini-app").await;
    let app = app(state(&harness));

    for (name, credential) in [
        ("no credential", None),
        ("an unknown credential", Some("not-a-real-credential")),
        (
            "a credential for another audience",
            Some("other-surface-credential"),
        ),
    ] {
        let (status, body) = send(&app, submit(credential, Some("k"), CAPTURE)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(body["code"], "platform.auth.unauthenticated", "{name}");
    }

    // And a revoked session stops working immediately, without waiting for expiry.
    let session_id: Uuid = sqlx::query_scalar(
        "select session_id from identity.sessions where audience = $1 order by issued_at limit 1",
    )
    .bind(AUDIENCE)
    .fetch_one(pool)
    .await
    .expect("the session");
    platform_identity::session::revoke_session(pool, session_id, now())
        .await
        .expect("revoking");

    let (status, _) = send(&app, submit(Some(CREDENTIAL), Some("k2"), CAPTURE)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a revoked session");

    harness.cleanup().await.expect("cleanup");
}

/// C-5. A replayable mutation without an idempotency key is refused rather than silently written.
#[tokio::test]
async fn a_capture_without_an_idempotency_key_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let (status, body) = send(&app, submit(Some(CREDENTIAL), None, CAPTURE)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "platform.request.idempotency_key_required");

    let operations: i64 = sqlx::query_scalar("select count(*) from operations.operations")
        .fetch_one(harness.pool())
        .await
        .expect("counting");
    assert_eq!(operations, 0, "a refused request must write nothing");

    harness.cleanup().await.expect("cleanup");
}

/// C-6. A body the route cannot act on is refused before an operation exists.
#[tokio::test]
async fn an_unusable_body_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    for body in [
        "not json at all",
        r#"{"wrong_field":"x"}"#,
        r#"{"url":"ftp://example.test/a"}"#,
        r#"{"url":"not a url"}"#,
    ] {
        let (status, envelope) = send(&app, submit(Some(CREDENTIAL), Some("k"), body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(envelope["code"], "platform.request.invalid", "{body}");
    }

    harness.cleanup().await.expect("cleanup");
}

/// C-7. Reading an operation returns the contract snapshot; another principal's operation and a
/// nonexistent one produce the same answer.
#[tokio::test]
async fn an_operation_is_readable_only_by_its_owner() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool, CREDENTIAL, AUDIENCE).await;
    seed(pool, "second-user-credential", AUDIENCE).await;
    let app = app(state(&harness));

    let (_, accepted) = send(&app, submit(Some(CREDENTIAL), Some("k"), CAPTURE)).await;
    let operation_id = accepted["operation_id"].as_str().expect("an id").to_owned();

    let read = |credential: &'static str, id: String| {
        let app = app.clone();
        async move {
            let request = Request::builder()
                .uri(format!("/v1/operations/{id}"))
                .header("authorization", format!("Bearer {credential}"))
                .body(Body::empty())
                .expect("a request");
            send(&app, request).await
        }
    };

    let (status, snapshot) = read(CREDENTIAL, operation_id.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["operation_id"], operation_id.as_str());
    assert_eq!(snapshot["status"], "accepted");
    assert_eq!(snapshot["kind"], "content.capture.submit");

    let (status, body) = read("second-user-credential", operation_id).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another principal's operation"
    );
    assert_eq!(body["code"], "platform.resource.not_found");

    let (missing, missing_body) = read(CREDENTIAL, Uuid::now_v7().to_string()).await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
    assert_eq!(
        missing_body["code"], "platform.resource.not_found",
        "a nonexistent operation and someone else's must be indistinguishable"
    );

    harness.cleanup().await.expect("cleanup");
}

/// C-12. A path segment that is not a UUID is the CLIENT's mistake, and is reported as one.
///
/// A regression guard for a defect milestone 8 found and fixed in the taxonomy rather than in a
/// route. `Path<Uuid>` rejects a malformed segment before any handler runs, with a 400 that no
/// handler authored — and `FailureKind::UNAUTHORED` had no entry for 400, so the boundary fell
/// through to its unmapped-status branch and answered **500**. Every route with a typed path
/// parameter had it, and nothing noticed because every test that reached those routes used a real
/// UUID.
#[tokio::test]
async fn a_malformed_path_parameter_is_a_client_error() {
    let harness = TestDatabase::create().await.expect("a test database");
    let credential = "path-parameter-credential-0000000";
    seed(harness.pool(), credential, AUDIENCE).await;
    let app = app(state(&harness));

    let request = Request::builder()
        .method("GET")
        .uri("/v1/operations/not-a-uuid")
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a request");
    let (status, body) = send(&app, request).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "platform.request.invalid");
    assert!(
        body["correlation_id"].is_string(),
        "an unauthored failure still carries the correlation the client saw"
    );
}

/// C-11. A submitted capture is in the audit trail, attributed to the session that submitted it,
/// and an anonymous attempt is not.
///
/// `identity.audit_events` and `audit::record` have existed since milestone 2. Milestone 8 gave them
/// their first writers on the authentication routes; this is the route that has been accepting work
/// since milestone 5 with no record of who asked for what. The unauthenticated half is asserted
/// because it is a decision rather than an oversight: a 401 with no credential has no actor to
/// attribute, and writing a row for one would let an unauthenticated caller grow the table.
#[tokio::test]
async fn an_accepted_capture_is_audited_and_an_anonymous_attempt_is_not() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let (status, body) = send(&app, submit(Some(CREDENTIAL), Some("audited-1"), CAPTURE)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let operation_id: Uuid = body["operation_id"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .expect("an operation id");

    let (status, _) = send(&app, submit(None, Some("audited-2"), CAPTURE)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let rows: Vec<(String, String, Uuid, Option<Uuid>)> = sqlx::query_as(
        "select action, outcome, actor_user_id, target_id from identity.audit_events",
    )
    .fetch_all(pool)
    .await
    .expect("the audit trail must read");

    assert_eq!(
        rows.len(),
        1,
        "one accepted capture, and nothing for the 401"
    );
    let (action, outcome, actor, target) = &rows[0];
    assert_eq!(action, "content.capture.submit");
    assert_eq!(outcome, "allowed");
    assert_eq!(*actor, user);
    assert_eq!(
        *target,
        Some(operation_id),
        "the target is the operation it created"
    );

    harness.cleanup().await.expect("cleanup");
}

/// C-12. The per-actor allowance is spent and the next request is refused with 429.
///
/// It is asserted on this route because the check is not on this route: it is in the `Principal`
/// extractor, which every authenticated route runs. Proving it here proves it for all of them, and
/// for the next one added.
#[tokio::test]
async fn an_actor_that_spends_its_allowance_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL, AUDIENCE).await;

    let mut state = state(&harness);
    state.actor_limit = Arc::new(platform_http::ActorLimiter::new(2));
    let app = app(state);

    for attempt in 1..=2 {
        let (status, _) = send(
            &app,
            submit(
                Some(CREDENTIAL),
                Some(&format!("limited-{attempt}")),
                CAPTURE,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "attempt {attempt}");
    }

    let (status, body) = send(&app, submit(Some(CREDENTIAL), Some("limited-3"), CAPTURE)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "platform.limit.rate_exceeded");
    assert_eq!(
        body["retryable"], true,
        "the allowance refills on its own, so the same request succeeds later",
    );

    harness.cleanup().await.expect("cleanup");
}
