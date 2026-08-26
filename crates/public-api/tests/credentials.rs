//! The credential-exchange routes: device login from the root secret, and rotation with replay
//! evidence.

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
use platform_identity::{NewSession, SecretDigest, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::ApiState;
use tower::ServiceExt as _;

const AUDIENCE: &str = "edge";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

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
        actor_requests_per_minute: 10_000,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(Arc::new(state)),
    )
}

/// A registered device with the given root secret.
async fn seed_device(pool: &sqlx::PgPool, secret: &str) -> (uuid::Uuid, uuid::Uuid) {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user")
        .user_id;
    let device = platform_identity::device::register_device(
        pool,
        user,
        platform_identity::DeviceKind::Mobile,
        None,
        SecretDigest::of(secret),
        now(),
    )
    .await
    .expect("a device");
    (user, device.device_id)
}

fn post(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
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
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

async fn audit_count(pool: &sqlx::PgPool, action: &str, outcome: &str) -> i64 {
    sqlx::query_scalar(
        "select count(*) from identity.audit_events where action = $1 and outcome = $2",
    )
    .bind(action)
    .bind(outcome)
    .fetch_one(pool)
    .await
    .expect("counting audits")
}

/// C-1. The root secret opens exactly one kind of door; every other presentation refuses alike.
#[tokio::test]
async fn device_login_opens_a_session_from_the_root_secret() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (_owner, device_id) = seed_device(pool, "the-root-secret").await;
    let app = app(state(&harness));

    let body = format!(r#"{{"device_id":"{device_id}","device_secret":"the-root-secret"}}"#);
    let (status, opened) = send(&app, post("/v1/sessions/device", body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let access = opened["credential"].as_str().expect("an access credential");
    assert_eq!(
        opened["device_id"].as_str(),
        Some(device_id.to_string()).as_deref()
    );

    let live =
        platform_identity::session::authenticate(pool, SecretDigest::of(access), AUDIENCE, now())
            .await
            .expect("authenticating")
            .expect("the granted credential works");
    assert_eq!(live.kind, SessionKind::Device);
    assert_eq!(live.device_id, Some(device_id));

    // Wrong secret, unknown device, revoked device: one refusal, three causes.
    let wrong = send(
        &app,
        post(
            "/v1/sessions/device",
            format!(r#"{{"device_id":"{device_id}","device_secret":"nope"}}"#),
        ),
    )
    .await;
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    let unknown = send(
        &app,
        post(
            "/v1/sessions/device",
            format!(
                r#"{{"device_id":"{}","device_secret":"whatever"}}"#,
                uuid::Uuid::now_v7()
            ),
        ),
    )
    .await;
    assert_eq!(unknown.0, StatusCode::UNAUTHORIZED);
    assert!(
        same_refusal(&wrong.1, &unknown.1),
        "wrong and unknown refuse identically"
    );

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::device::revoke_device(&mut transaction, device_id, now())
        .await
        .expect("revoking");
    transaction.commit().await.expect("commit");
    let revoked = send(
        &app,
        post(
            "/v1/sessions/device",
            format!(r#"{{"device_id":"{device_id}","device_secret":"the-root-secret"}}"#),
        ),
    )
    .await;
    assert_eq!(revoked.0, StatusCode::UNAUTHORIZED);
    assert!(
        same_refusal(&revoked.1, &wrong.1),
        "revoked refuses identically too"
    );

    assert_eq!(audit_count(pool, "session.open_device", "allowed").await, 1);
    assert!(audit_count(pool, "session.open_device", "denied").await >= 3);

    harness.cleanup().await.expect("cleanup");
}

/// C-2. Rotation swaps both credentials; presenting a spent link burns its family invisibly.
#[tokio::test]
async fn refresh_rotates_and_replay_burns_the_family() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, device_id) = seed_device(pool, "another-root").await;
    let issued = now();
    let session = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: owner,
            kind: SessionKind::Device,
            device_id: Some(device_id),
            audience: AUDIENCE,
            token: Some(SecretDigest::of("access-v1")),
            issued_at: issued,
            expires_at: issued + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    platform_identity::session::issue_refresh_token(
        pool,
        session.session_id,
        SecretDigest::of("link-1"),
        issued,
        issued + jiff::SignedDuration::from_hours(24 * 30),
    )
    .await
    .expect("a refresh link");

    let app = app(state(&harness));

    let (status, rotated) = send(
        &app,
        post(
            "/v1/sessions/refresh",
            r#"{"refresh_token":"link-1"}"#.to_owned(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let new_access = rotated["credential"]
        .as_str()
        .expect("a new access credential");
    let new_link = rotated["refresh_token"].as_str().expect("a new link");
    assert_ne!(new_access, "access-v1");
    assert_ne!(new_link, "link-1");

    // The old credential is dead; the new one authenticates.
    let old = platform_identity::session::authenticate(
        pool,
        SecretDigest::of("access-v1"),
        AUDIENCE,
        now(),
    )
    .await
    .expect("authenticating");
    assert!(old.is_none(), "the replaced credential must be dead");
    let fresh = platform_identity::session::authenticate(
        pool,
        SecretDigest::of(new_access),
        AUDIENCE,
        now(),
    )
    .await
    .expect("authenticating")
    .expect("the replacement works");
    assert!(
        fresh.expires_at > session.expires_at,
        "rotation extends the window"
    );
}

/// C-2b. Presenting a spent link is indistinguishable from outside and burns the family.
#[tokio::test]
async fn refresh_replay_burns_the_family_invisibly() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, device_id) = seed_device(pool, "another-root").await;
    let issued = now();
    let session = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: owner,
            kind: SessionKind::Device,
            device_id: Some(device_id),
            audience: AUDIENCE,
            token: Some(SecretDigest::of("access-v1")),
            issued_at: issued,
            expires_at: issued + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    platform_identity::session::issue_refresh_token(
        pool,
        session.session_id,
        SecretDigest::of("link-1"),
        issued,
        issued + jiff::SignedDuration::from_hours(24 * 30),
    )
    .await
    .expect("a refresh link");

    // Spend it once through the route; the successor it minted is what a replay must burn.
    let app = app(state(&harness));
    let (status, rotated) = send(
        &app,
        post(
            "/v1/sessions/refresh",
            r#"{"refresh_token":"link-1"}"#.to_owned(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let new_access = rotated["credential"]
        .as_str()
        .expect("a new access credential")
        .to_owned();

    // Replaying the spent link: the same refusal as any other failure, and the family burns.
    let (replay_status, replay_answer) = send(
        &app,
        post(
            "/v1/sessions/refresh",
            r#"{"refresh_token":"link-1"}"#.to_owned(),
        ),
    )
    .await;
    assert_eq!(replay_status, StatusCode::UNAUTHORIZED);
    let (unknown_status, unknown_answer) = send(
        &app,
        post(
            "/v1/sessions/refresh",
            r#"{"refresh_token":"never-existed"}"#.to_owned(),
        ),
    )
    .await;
    assert_eq!(unknown_status, StatusCode::UNAUTHORIZED);
    assert!(
        same_refusal(&replay_answer, &unknown_answer),
        "replay is indistinguishable from outside"
    );
    let burned = platform_identity::session::authenticate(
        pool,
        SecretDigest::of(&new_access),
        AUDIENCE,
        now(),
    )
    .await
    .expect("authenticating");
    assert!(burned.is_none(), "the replay burned the whole family");
    let whys = platform_identity::count_revocations(
        pool,
        platform_identity::RevocationSubject::Session,
        session.session_id,
    )
    .await
    .expect("counting revocations");
    assert_eq!(whys, 1, "the burn recorded its why");

    assert_eq!(audit_count(pool, "session.refresh", "allowed").await, 1);
    assert_eq!(audit_count(pool, "session.refresh", "denied").await, 2);

    harness.cleanup().await.expect("cleanup");
}

/// Two refusal envelopes are THE SAME when everything but the per-request correlation matches.
fn same_refusal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    let strip = |v: &serde_json::Value| {
        let mut owned = v.clone();
        if let Some(object) = owned.as_object_mut() {
            object.remove("correlation_id");
        }
        owned
    };
    match (
        serde_json::to_string(&strip(a)),
        serde_json::to_string(&strip(b)),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
