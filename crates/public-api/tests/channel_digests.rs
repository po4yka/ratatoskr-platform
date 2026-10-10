//! The channel-digest command routes (XR-021 CONTRACTS.md S08): a subscription change and an
//! on-demand run are accepted as Platform operations and handed to the digest service as typed
//! `CommandEnvelope`s, in the same transaction as the idempotency reservation.

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
use ratatoskr_channel_digest_contracts::{
    ChannelDigestRunRequested, ChannelDigestRunTrigger, ChannelDigestSubscriptionSetRequested,
    OutputLanguage, SubscriptionDesiredState,
};
use ratatoskr_event_envelope::CommandEnvelope;
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "channel-digest-credential-00000000";
const AUDIENCE: &str = "edge";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

fn app(harness: &TestDatabase) -> Router {
    let health = Arc::new(platform_http::RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
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
        platform_public_api::routes(Arc::new(state)),
    )
}

async fn seed(pool: &sqlx::PgPool) -> Uuid {
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
            token: Some(auth::credential_digest(CREDENTIAL)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
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

fn put_subscription(username: &str, key: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/channel-digests/subscriptions/{username}"))
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

fn post_run(key: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/channel-digests/runs")
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

fn operation_of(body: &serde_json::Value) -> Uuid {
    body["operation_id"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .expect("an operation id")
}

/// The outbox rows of an operation: message id, subject and payload.
async fn outbox_rows(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
) -> Vec<(Uuid, String, serde_json::Value)> {
    sqlx::query_as(
        "select message_id, subject, payload from operations.outbox where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_all(pool)
    .await
    .expect("the outbox")
}

async fn total_outbox_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from operations.outbox")
        .fetch_one(pool)
        .await
        .expect("counting commands")
}

#[tokio::test]
async fn put_subscription_accepts_and_enqueues_a_contract_command() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed(pool).await;
    let app = app(&harness);

    let (status, body) = send(
        &app,
        put_subscription(
            "example_channel",
            Some("subscribe-1"),
            r#"{"desired_state":"active"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "accepted");
    let operation_id = operation_of(&body);

    let operation = platform_operations::find(pool, operation_id)
        .await
        .expect("reading")
        .expect("the operation");
    assert_eq!(operation.kind, "channel_digest.subscription.set");
    assert_eq!(operation.owner_user_id, user);

    let rows = outbox_rows(pool, operation_id).await;
    assert_eq!(rows.len(), 1, "exactly one command");
    let (message_id, subject, stored) = &rows[0];
    assert_eq!(subject, "cmd.channel_digest.subscription.set_requested.v1");

    let envelope = CommandEnvelope::from_json(stored.to_string().as_bytes())
        .expect("the stored row is a contract CommandEnvelope");
    assert_eq!(
        stored["command_id"],
        message_id.to_string(),
        "the outbox message id is the command id"
    );
    assert_eq!(stored["producer"], "ratatoskr-platform");
    assert_eq!(stored["aggregate_id"], format!("operation:{operation_id}"));
    assert_eq!(
        stored["correlation_id"],
        format!("operation:{operation_id}")
    );
    assert_eq!(stored["tenant_id"], format!("user:{user}"));
    let payload: ChannelDigestSubscriptionSetRequested = envelope
        .payload_as()
        .expect("the payload is a subscription command");
    payload
        .validate_for_publish()
        .expect("a publishable command");
    assert_eq!(payload.operation_id.0, operation_id);
    assert_eq!(payload.channel_username.as_str(), "example_channel");
    assert_eq!(payload.desired_state, SubscriptionDesiredState::Active);
    assert_eq!(
        stored["payload"]["owner"], stored["tenant_id"],
        "the tenant equals the payload owner"
    );
    assert_eq!(
        stored["payload"]["idempotency_key"],
        format!("operation.{operation_id}")
    );
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn post_run_window_is_24h_closed_open_and_trigger_accepted_at_equals_end() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed(pool).await;
    let app = app(&harness);

    let (status, body) = send(&app, post_run(Some("run-1"), r#"{"output_language":"ru"}"#)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let operation_id = operation_of(&body);
    let operation = platform_operations::find(pool, operation_id)
        .await
        .expect("reading")
        .expect("the operation");
    assert_eq!(operation.kind, "channel_digest.run");

    let rows = outbox_rows(pool, operation_id).await;
    assert_eq!(rows.len(), 1);
    let (message_id, subject, stored) = &rows[0];
    assert_eq!(subject, "cmd.channel_digest.run.requested.v1");
    let envelope = CommandEnvelope::from_json(stored.to_string().as_bytes()).expect("an envelope");
    let payload: ChannelDigestRunRequested = envelope.payload_as().expect("a run command");
    payload
        .validate_for_publish()
        .expect("a publishable command");

    assert_eq!(stored["command_id"], message_id.to_string());
    assert_eq!(stored["producer"], "ratatoskr-platform");
    assert_eq!(stored["tenant_id"], format!("user:{user}"));
    assert_eq!(
        stored["correlation_id"],
        format!("operation:{operation_id}")
    );
    assert_eq!(
        stored["aggregate_id"],
        format!("channel-digest-run:{}", payload.digest_run_id)
    );
    assert_eq!(payload.output_language, OutputLanguage::Ru);
    assert_eq!(
        payload
            .window
            .end_at
            .as_jiff()
            .duration_since(payload.window.start_at.as_jiff()),
        jiff::SignedDuration::from_hours(24),
        "the window is the 24 hours before acceptance"
    );
    let ChannelDigestRunTrigger::OnDemand { accepted_at } = payload.trigger else {
        panic!("an on-demand request has an on-demand trigger");
    };
    assert_eq!(
        accepted_at, payload.window.end_at,
        "the window is closed-open and ends at the acceptance instant"
    );
    assert_eq!(
        stored["payload"]["idempotency_key"],
        format!("operation.{operation_id}")
    );
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn same_idempotency_key_replays_the_original_operation_without_a_second_outbox_row() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool).await;
    let app = app(&harness);

    let subscription = || {
        put_subscription(
            "example_channel",
            Some("replayed"),
            r#"{"desired_state":"inactive"}"#,
        )
    };
    let (first_status, first) = send(&app, subscription()).await;
    let (second_status, second) = send(&app, subscription()).await;
    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(second_status, StatusCode::ACCEPTED);
    assert_eq!(first["operation_id"], second["operation_id"]);

    let run = || post_run(Some("run-replayed"), r#"{"output_language":"en"}"#);
    let (_, first_run) = send(&app, run()).await;
    let (_, second_run) = send(&app, run()).await;
    assert_eq!(first_run["operation_id"], second_run["operation_id"]);

    assert_eq!(
        total_outbox_rows(pool).await,
        2,
        "one command per distinct request, none for a replay"
    );
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn same_key_different_body_is_409() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool).await;
    let app = app(&harness);

    send(
        &app,
        put_subscription(
            "example_channel",
            Some("conflict"),
            r#"{"desired_state":"active"}"#,
        ),
    )
    .await;
    let (status, body) = send(
        &app,
        put_subscription(
            "example_channel",
            Some("conflict"),
            r#"{"desired_state":"inactive"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_conflict");

    send(
        &app,
        post_run(Some("run-conflict"), r#"{"output_language":"ru"}"#),
    )
    .await;
    let (status, body) = send(
        &app,
        post_run(Some("run-conflict"), r#"{"output_language":"en"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn uppercase_or_short_username_is_400() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool).await;
    let app = app(&harness);

    for (index, username) in ["Example_Channel", "abcd", "1example", "has-dash_x"]
        .into_iter()
        .enumerate()
    {
        let (status, body) = send(
            &app,
            put_subscription(
                username,
                Some(&format!("bad-username-{index}")),
                r#"{"desired_state":"active"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{username}: {body}");
        assert_eq!(body["code"], "platform.request.invalid");
    }
    let (status, body) = send(
        &app,
        put_subscription(
            "example_channel",
            Some("bad-state"),
            r#"{"desired_state":"paused"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(
        &app,
        post_run(Some("bad-language"), r#"{"output_language":"fr"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(&app, post_run(Some("no-language"), "{}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    assert_eq!(total_outbox_rows(pool).await, 0, "a refusal writes nothing");
    let operations: i64 = sqlx::query_scalar("select count(*) from operations.operations")
        .fetch_one(pool)
        .await
        .expect("counting");
    assert_eq!(operations, 0);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn missing_idempotency_key_is_400() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool).await;
    let app = app(&harness);

    let (status, body) = send(
        &app,
        put_subscription("example_channel", None, r#"{"desired_state":"active"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_key_required");
    let (status, body) = send(&app, post_run(None, r#"{"output_language":"ru"}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_key_required");
    assert_eq!(total_outbox_rows(pool).await, 0);
    harness.cleanup().await.expect("cleanup");
}
