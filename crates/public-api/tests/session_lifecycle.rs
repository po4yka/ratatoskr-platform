//! The session lifecycle routes: listing, single revocation, revoke-all.

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
use platform_public_api::{ApiState, auth};
use tower::ServiceExt as _;
use uuid::Uuid;

const AUDIENCE: &str = "edge";
const ALICE_CREDENTIAL: &str = "lifecycle-alice-credential";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn ago(minutes: i64) -> jiff::Timestamp {
    now() - jiff::SignedDuration::from_mins(minutes)
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

/// One user with one live browser session authenticating as `ALICE_CREDENTIAL`.
async fn seed_alice(pool: &sqlx::PgPool) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user")
        .user_id;
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user,
            kind: SessionKind::Browser,
            device_id: None,
            audience: AUDIENCE,
            token: Some(auth::credential_digest(ALICE_CREDENTIAL)),
            issued_at: ago(10),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user
}

/// An extra live session for `user`, authenticating with the returned credential.
async fn extra_session(pool: &sqlx::PgPool, user: Uuid, kind: SessionKind) -> (Uuid, String) {
    let credential = format!("extra-{}", uuid::Uuid::now_v7());
    let session = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user,
            kind,
            device_id: None,
            audience: AUDIENCE,
            token: Some(auth::credential_digest(&credential)),
            issued_at: ago(5),
            expires_at: now() + jiff::SignedDuration::from_hours(2),
        },
    )
    .await
    .expect("a session");
    (session.session_id, credential)
}

fn get(uri: &str, credential: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder.body(Body::empty()).expect("a request")
}

fn send_delete(uri: &str, credential: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a request")
}

fn send_post(uri: &str, credential: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
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

/// S-1. The listing shows only the caller's live sessions, newest first, cursor-paginated.
#[tokio::test]
async fn session_listing_is_scoped_paginated_and_live_only() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let alice = seed_alice(pool).await;
    let bob = platform_identity::user::create_user(pool, now())
        .await
        .expect("another user")
        .user_id;

    // Alice: a second live session, one revoked, one expired. Bob: one live.
    let (_second, _) = extra_session(pool, alice, SessionKind::TelegramMiniApp).await;
    let dead = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: alice,
            kind: SessionKind::Browser,
            device_id: None,
            audience: AUDIENCE,
            token: Some(auth::credential_digest("dead-credential")),
            issued_at: ago(30),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a doomed session");
    platform_identity::session::revoke_session(pool, dead.session_id, ago(1))
        .await
        .expect("revoking");
    let _expired = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: alice,
            kind: SessionKind::Browser,
            device_id: None,
            audience: AUDIENCE,
            token: None,
            issued_at: ago(90),
            expires_at: ago(60),
        },
    )
    .await
    .expect("an expired session");
    let (_bobs, _) = extra_session(pool, bob, SessionKind::Browser).await;

    let app = app(state(&harness));

    let (status, _) = send(&app, get("/v1/sessions", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, page_one) = send(&app, get("/v1/sessions?limit=1", Some(ALICE_CREDENTIAL))).await;
    assert_eq!(status, StatusCode::OK);
    let rows = page_one["sessions"].as_array().expect("a page");
    assert_eq!(rows.len(), 1, "the limit bounds the page");
    assert_eq!(
        rows[0]["kind"].as_str(),
        Some("telegram_mini_app"),
        "newest first"
    );
    assert!(rows[0]["issued_at"].as_str().is_some());
    assert!(rows[0]["expires_at"].as_str().is_some());
    assert!(rows[0].get("last_seen_at").is_some());

    let cursor = page_one["next_cursor"].as_str().expect("more pages exist");
    let (status, page_two) = send(
        &app,
        get(
            &format!("/v1/sessions?limit=10&cursor={cursor}"),
            Some(ALICE_CREDENTIAL),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rest = page_two["sessions"].as_array().expect("a page");
    assert_eq!(
        rest.len(),
        1,
        "exactly one more LIVE session exists: revoked and expired never appear"
    );
    let listed_ids: Vec<String> = rows
        .iter()
        .chain(rest.iter())
        .filter_map(|s| s["session_id"].as_str())
        .map(str::to_owned)
        .collect();
    assert!(
        !listed_ids.iter().any(|id| id == &bob.to_string()),
        "tenant isolation"
    );

    harness.cleanup().await.expect("cleanup");
}

/// S-2. Single revocation is owner-scoped, truthful on repeats, and audited on both sides.
#[tokio::test]
async fn session_revocation_is_scoped_truthful_and_audited() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let alice = seed_alice(pool).await;
    let bob = platform_identity::user::create_user(pool, now())
        .await
        .expect("another user")
        .user_id;
    let (bobs_session, _) = extra_session(pool, bob, SessionKind::Browser).await;
    let (second, second_credential) = extra_session(pool, alice, SessionKind::Browser).await;
    let app = app(state(&harness));

    // Foreign target: 404 with a denial audited; a pure miss: 404 with no new denial.
    let denied_before = audit_count(pool, "session.revoke", "denied").await;
    let (status, _) = send(
        &app,
        send_delete(&format!("/v1/sessions/{bobs_session}"), ALICE_CREDENTIAL),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        audit_count(pool, "session.revoke", "denied").await,
        denied_before + 1
    );

    let (status, _) = send(
        &app,
        send_delete(
            &format!("/v1/sessions/{}", Uuid::now_v7()),
            ALICE_CREDENTIAL,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        audit_count(pool, "session.revoke", "denied").await,
        denied_before + 1
    );

    // Own target: 204, access ends, why recorded, grant audited.
    let allowed_before = audit_count(pool, "session.revoke", "allowed").await;
    let (status, _) = send(
        &app,
        send_delete(&format!("/v1/sessions/{second}"), ALICE_CREDENTIAL),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        audit_count(pool, "session.revoke", "allowed").await,
        allowed_before + 1
    );

    let (status, _) = send(&app, get("/v1/sessions", Some(&second_credential))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the revoked credential is dead"
    );

    // Repeat: the same 404 as everything else that is not a live session of yours.
    let (status, _) = send(
        &app,
        send_delete(&format!("/v1/sessions/{second}"), ALICE_CREDENTIAL),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let whys = platform_identity::count_revocations(
        pool,
        platform_identity::RevocationSubject::Session,
        second,
    )
    .await
    .expect("counting revocations");
    assert_eq!(whys, 1);

    harness.cleanup().await.expect("cleanup");
}

/// S-3. Revoke-all sweeps every kind including the caller and spares devices.
#[tokio::test]
async fn revoke_all_sweeps_every_kind_and_spares_devices() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let alice = seed_alice(pool).await;
    let bob = platform_identity::user::create_user(pool, now())
        .await
        .expect("another user")
        .user_id;
    let (_a2, a2_credential) = extra_session(pool, alice, SessionKind::TelegramMiniApp).await;
    let (_bobs, bobs_credential) = extra_session(pool, bob, SessionKind::Browser).await;

    // A paired device holding a live session.
    let device = platform_identity::device::register_device(
        pool,
        alice,
        platform_identity::DeviceKind::Mobile,
        None,
        SecretDigest::of("surviving-root"),
        now(),
    )
    .await
    .expect("a device");
    let (_device_session_id, device_access) = {
        let credential = "device-access-cred".to_owned();
        let session = platform_identity::session::create_session(
            pool,
            &NewSession {
                user_id: alice,
                kind: SessionKind::Device,
                device_id: Some(device.device_id),
                audience: AUDIENCE,
                token: Some(auth::credential_digest(&credential)),
                issued_at: ago(2),
                expires_at: now() + jiff::SignedDuration::from_hours(1),
            },
        )
        .await
        .expect("a device session");
        (session.session_id, credential)
    };

    let app = app(state(&harness));
    let (status, swept) = send(&app, send_post("/v1/sessions/revoke-all", ALICE_CREDENTIAL)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        swept["revoked"].as_u64(),
        Some(3),
        "every live session, caller included"
    );

    for credential in [
        ALICE_CREDENTIAL,
        a2_credential.as_str(),
        device_access.as_str(),
    ] {
        let (status, _) = send(&app, get("/v1/sessions", Some(credential))).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{credential} must be dead"
        );
    }

    // Bob is untouched.
    let (status, _) = send(&app, get("/v1/sessions", Some(bobs_credential.as_str()))).await;
    assert_eq!(status, StatusCode::OK);

    // Each swept session carries its durable why.
    for session_id in alice_sessions(pool, alice).await {
        let whys = platform_identity::count_revocations(
            pool,
            platform_identity::RevocationSubject::Session,
            session_id,
        )
        .await
        .expect("counting revocations");
        assert_eq!(whys, 1, "session {session_id} records exactly one why");
    }

    // And the device recovers through its root secret — killing logins did not brick it.
    let (status, opened) = send(
        &app,
        post_json(
            "/v1/sessions/device".to_owned(),
            format!(
                r#"{{"device_id":"{}","device_secret":"surviving-root"}}"#,
                device.device_id
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "the device logs back in");
    let fresh_access = opened["credential"].as_str().expect("a fresh credential");
    let live = platform_identity::session::authenticate(
        pool,
        SecretDigest::of(fresh_access),
        AUDIENCE,
        now(),
    )
    .await
    .expect("authenticating");
    assert!(
        live.is_some(),
        "the recovered credential authenticates a new session"
    );

    harness.cleanup().await.expect("cleanup");
}

async fn alice_sessions(pool: &sqlx::PgPool, user: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar("select session_id from identity.sessions where user_id = $1")
        .bind(user)
        .fetch_all(pool)
        .await
        .expect("alice's sessions")
}

fn post_json(uri: impl Into<String>, body: String) -> Request<Body> {
    let uri = uri.into();
    Request::builder()
        .method("POST")
        .uri(&uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("a request")
}
