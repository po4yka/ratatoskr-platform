//! The device routes, end to end against real `PostgreSQL`: pairing codes, pairing, listing,
//! deletion.

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
use platform_identity::{DeviceKind, NewSession, SecretDigest, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tower::ServiceExt as _;
use uuid::Uuid;

const AUDIENCE: &str = "edge";
const OWNER_CREDENTIAL: &str = "devices-owner-credential";

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

/// A user with one live browser session: the trusted context pairing starts from.
async fn seed_owner(pool: &sqlx::PgPool) -> uuid::Uuid {
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
            token: Some(auth::credential_digest(OWNER_CREDENTIAL)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
}

fn post(uri: &str, credential: Option<&str>, body: String) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder.body(Body::from(body)).expect("a request")
}

fn get(uri: &str, credential: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder.body(Body::empty()).expect("a request")
}

fn delete(uri: &str, credential: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("DELETE").uri(uri);
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder.body(Body::empty()).expect("a request")
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

async fn mint_code(app: &Router, expected_kind: Option<&str>) -> (StatusCode, serde_json::Value) {
    let body = match expected_kind {
        Some(kind) => format!(r#"{{"expected_kind":"{kind}","label":"pixel phone"}}"#),
        None => r#"{"label":"anything"}"#.to_owned(),
    };
    send(
        app,
        post("/v1/devices/pairing-codes", Some(OWNER_CREDENTIAL), body),
    )
    .await
}

/// D-1. Minting is authenticated, returns a one-time code, supersedes the previous pending code,
/// and audits the grant.
#[tokio::test]
async fn pairing_code_creation_is_authenticated_superseding_and_audited() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_owner(pool).await;
    let app = app(state(&harness));

    // Unauthenticated minting is simply refused.
    let (status, _) = send(&app, post("/v1/devices/pairing-codes", None, "{}".into())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, first) = mint_code(&app, Some("mobile")).await;
    assert_eq!(status, StatusCode::CREATED);
    let code = first["code"].as_str().expect("a code");
    assert!(code.len() >= 20, "the code carries real entropy");
    assert!(first["expires_at"].as_str().is_some());

    let (status, second) = mint_code(&app, None).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(
        second["code"].as_str().expect("a second code"),
        code,
        "every mint is fresh randomness"
    );

    // The previous pending code was set aside, observable in storage.
    let superseded: i64 = sqlx::query_scalar(
        "select count(*) from identity.pairing_codes where user_id = $1 and superseded_at is not null",
    )
    .bind(owner)
    .fetch_one(pool)
    .await
    .expect("reading codes");
    assert_eq!(superseded, 1);

    assert_eq!(
        audit_count(pool, "device.pairing_code_create", "allowed").await,
        2
    );

    harness.cleanup().await.expect("cleanup");
}

/// D-1b. A paired device is not a primary session and cannot bootstrap another installation.
#[tokio::test]
async fn a_device_session_cannot_create_a_pairing_code() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_owner(pool).await;
    let device = platform_identity::device::register_device(
        pool,
        owner,
        DeviceKind::Mobile,
        Some("child"),
        platform_identity::SecretDigest::of("child-root"),
        now(),
    )
    .await
    .expect("a device");
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: owner,
            kind: SessionKind::Device,
            device_id: Some(device.device_id),
            audience: AUDIENCE,
            token: Some(auth::credential_digest("child-access")),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a device session");
    let app = app(state(&harness));

    let (status, _) = send(
        &app,
        post(
            "/v1/devices/pairing-codes",
            Some("child-access"),
            r#"{"expected_kind":"mobile","label":"grandchild"}"#.to_owned(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        audit_count(pool, "device.pairing_code_create", "denied").await,
        1
    );

    harness.cleanup().await.expect("cleanup");
}

/// D-2. A live code grants exactly once; every unacceptable presentation refuses identically.
#[tokio::test]
async fn pairing_exchanges_once_and_refuses_identically() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed_owner(pool).await;
    let app = app(state(&harness));

    let (_, issued) = mint_code(&app, Some("mobile")).await;
    let code = issued["code"].as_str().expect("a code").to_owned();

    // Wrong kind first: refused WITHOUT burning the code.
    let wrong_body =
        format!(r#"{{"code":"{code}","kind":"browser_extension","display_name":"x"}}"#);
    let (wrong_status, wrong_body_answer) =
        send(&app, post("/v1/devices/pair", None, wrong_body.clone())).await;
    assert_eq!(wrong_status, StatusCode::UNAUTHORIZED);

    // Now the correct kind: granted once, everything at once.
    let good_body = format!(r#"{{"code":"{code}","kind":"mobile","display_name":"pixel"}}"#);
    let (status, paired) = send(&app, post("/v1/devices/pair", None, good_body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let device_id = paired["device_id"].as_str().expect("a device id");
    let access = paired["credential"].as_str().expect("an access credential");
    let refresh = paired["refresh_token"].as_str().expect("a refresh token");
    let secret = paired["device_secret"].as_str().expect("a device secret");
    assert!(!access.is_empty() && !refresh.is_empty() && !secret.is_empty());

    // Storage agrees: an active mobile device under the owner, a bound live session with a
    // refresh chain of exactly one link, and a consumed code naming the device.
    let row: (i64,) = sqlx::query_as(
        "select count(*) from identity.registered_devices d
          join identity.sessions s on s.device_id = d.device_id and s.revoked_at is null
          join identity.refresh_tokens t on t.session_id = s.session_id
         where d.device_id = $1 and d.kind = 'mobile' and d.display_name = 'pixel'",
    )
    .bind(Uuid::parse_str(device_id).expect("a uuid"))
    .fetch_one(pool)
    .await
    .expect("the joined grant");
    assert_eq!(row.0, 1);
    let consumed: Option<Uuid> = sqlx::query_scalar(
        "select consumed_by_device_id from identity.pairing_codes where consumed_at is not null",
    )
    .fetch_optional(pool)
    .await
    .expect("reading the consumed code");
    assert_eq!(consumed, Some(Uuid::parse_str(device_id).expect("a uuid")));
    let session_live =
        platform_identity::session::authenticate(pool, SecretDigest::of(access), AUDIENCE, now())
            .await
            .expect("authenticating");
    assert!(
        session_live.is_some(),
        "the granted credential authenticates"
    );

    // Every refusal from here is THE refusal: replayed, unknown, and the earlier kind mismatch.
    let (replay_status, replay_answer) = send(
        &app,
        post(
            "/v1/devices/pair",
            None,
            format!(r#"{{"code":"{code}","kind":"mobile","display_name":"pixel"}}"#),
        ),
    )
    .await;
    assert_eq!(replay_status, StatusCode::UNAUTHORIZED);
    let unknown_body = r#"{"code":"AAAAAAAAAAAAAAAAAAAAAA","kind":"mobile"}"#.to_owned();
    let (unknown_status, unknown_answer) =
        send(&app, post("/v1/devices/pair", None, unknown_body)).await;
    assert_eq!(unknown_status, StatusCode::UNAUTHORIZED);
    assert!(
        same_refusal(&replay_answer, &wrong_body_answer),
        "the kind-mismatched refusal looks like this one"
    );
    assert!(
        same_refusal(&replay_answer, &unknown_answer),
        "an unknown code refuses identically to a spent one"
    );
    assert_eq!(
        replay_answer["code"].as_str(),
        Some(
            platform_core::FailureKind::Unauthenticated
                .fault()
                .code
                .as_str(),
        ),
        "and it is the contract unauthenticated fault"
    );

    // Grants and denials both reached the audit trail.
    assert_eq!(audit_count(pool, "device.pair", "allowed").await, 1);
    assert!(audit_count(pool, "device.pair", "denied").await >= 3);

    harness.cleanup().await.expect("cleanup");
}

/// Devices on file for the owner and one foreign user, fresh per call.
async fn seed_device_fixture(pool: &sqlx::PgPool, owner: uuid::Uuid) -> (Vec<Uuid>, Uuid) {
    let mut own = Vec::new();
    for index in 0..3_u8 {
        let device = platform_identity::device::register_device(
            pool,
            owner,
            platform_identity::DeviceKind::Mobile,
            Some(&format!("phone {index}")),
            SecretDigest::of(&format!("device-secret-{index}")),
            now(),
        )
        .await
        .expect("a device");
        own.push(device.device_id);
    }
    let other = platform_identity::user::create_user(pool, now())
        .await
        .expect("another user")
        .user_id;
    let foreign = platform_identity::device::register_device(
        pool,
        other,
        platform_identity::DeviceKind::ExportAgent,
        None,
        SecretDigest::of("foreign-secret"),
        now(),
    )
    .await
    .expect("a foreign device");
    (own, foreign.device_id)
}

/// D-3a. The listing isolates tenants and paginates deterministically.
#[tokio::test]
async fn device_listing_is_scoped_and_paginated() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_owner(pool).await;
    let app = app(state(&harness));
    let (own, foreign) = seed_device_fixture(pool, owner).await;

    let (status, page_one) = send(&app, get("/v1/devices?limit=2", Some(OWNER_CREDENTIAL))).await;
    assert_eq!(status, StatusCode::OK);
    let rows_one: Vec<String> = page_one["devices"]
        .as_array()
        .expect("a page")
        .iter()
        .filter_map(|d| d["device_id"].as_str())
        .map(str::to_owned)
        .collect();
    assert_eq!(rows_one.len(), 2);
    assert!(!rows_one.iter().any(|id| id == &foreign.to_string()));
    let cursor = page_one["next_cursor"].as_str().expect("more pages exist");

    let (status, page_two) = send(
        &app,
        get(
            &format!("/v1/devices?limit=2&cursor={cursor}"),
            Some(OWNER_CREDENTIAL),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut all_seen = rows_one;
    all_seen.extend(
        page_two["devices"]
            .as_array()
            .expect("a page")
            .iter()
            .filter_map(|d| d["device_id"].as_str())
            .map(str::to_owned),
    );
    all_seen.sort();
    let mut expected = own
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        all_seen, expected,
        "the walk visits each active device exactly once"
    );

    harness.cleanup().await.expect("cleanup");
}

/// D-3b. Deletion is owner-scoped, audits denials against real foreign targets, cascades fully.
#[tokio::test]
async fn device_deletion_is_scoped_truthful_and_cascades() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_owner(pool).await;
    let app = app(state(&harness));
    let (_own, foreign) = seed_device_fixture(pool, owner).await;

    // Foreign target: 404 indistinguishable from a miss, yet audited as a denial.
    let denied_before = audit_count(pool, "device.revoke", "denied").await;
    let (status, _) = send(
        &app,
        delete(&format!("/v1/devices/{foreign}"), Some(OWNER_CREDENTIAL)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        audit_count(pool, "device.revoke", "denied").await,
        denied_before + 1
    );

    // A pure miss adds no denial: there is nothing to attribute.
    let (status, _) = send(
        &app,
        delete(
            &format!("/v1/devices/{}", Uuid::now_v7()),
            Some(OWNER_CREDENTIAL),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        audit_count(pool, "device.revoke", "denied").await,
        denied_before + 1
    );

    // Own target: cascade, revocation whys, audit grant, dead root secret.
    let target = platform_identity::device::register_device(
        pool,
        owner,
        platform_identity::DeviceKind::BrowserExtension,
        None,
        SecretDigest::of("target-root"),
        now(),
    )
    .await
    .expect("a target device");
    let bound_session = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: owner,
            kind: SessionKind::Device,
            device_id: Some(target.device_id),
            audience: AUDIENCE,
            token: Some(SecretDigest::of("target-access")),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a bound session");

    let allowed_before = audit_count(pool, "device.revoke", "allowed").await;
    let (status, _) = send(
        &app,
        delete(
            &format!("/v1/devices/{}", target.device_id),
            Some(OWNER_CREDENTIAL),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        audit_count(pool, "device.revoke", "allowed").await,
        allowed_before + 1
    );

    let still_live = platform_identity::session::authenticate(
        pool,
        SecretDigest::of("target-access"),
        AUDIENCE,
        now(),
    )
    .await
    .expect("authenticating");
    assert!(still_live.is_none(), "the cascade killed the bound session");
    let root_works = platform_identity::device::verify_device_secret(
        pool,
        target.device_id,
        SecretDigest::of("target-root"),
    )
    .await
    .expect("verifying");
    assert!(!root_works, "the root secret died with the device");
    let whys: i64 =
        sqlx::query_scalar("select count(*) from identity.revocations where subject_id = any($1)")
            .bind(&[target.device_id, bound_session.session_id][..])
            .fetch_one(pool)
            .await
            .expect("counting revocations");
    assert_eq!(whys, 2, "both subjects carry their durable why");

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
