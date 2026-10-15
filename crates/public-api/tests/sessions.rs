//! `POST /v1/sessions/telegram` — tests X-1 … X-6.
//!
//! The route is how a caller becomes authenticated, so it is unauthenticated itself and everything
//! about it has to hold on that basis. ADR-0011 is the design; these are its consequences.

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
use platform_core::config::PublicConfig;
use platform_core::{Capability, RuntimeRole};
use platform_http::{HttpState, RuntimeState};
use platform_identity::assertion::{self, AssertionClaims, TELEGRAM_ISSUER};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::ApiState;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use tower::ServiceExt as _;
use uuid::Uuid;

mod site_owner_support;

const AUDIENCE: &str = "edge";

/// The handlers read the wall clock themselves — the API state carries no clock — so a session
/// or grant seeded here has to be minted on that same clock to be live when the handler checks it.
fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

fn issuer() -> Ed25519KeyPair {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("a key pair");
    Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("a usable key pair")
}

fn claims(subject: &str) -> AssertionClaims {
    AssertionClaims {
        issuer: TELEGRAM_ISSUER.to_owned(),
        subject: subject.to_owned(),
        audience: AUDIENCE.to_owned(),
        nonce: Uuid::now_v7().simple().to_string(),
        issued_at: now(),
        expires_at: now() + jiff::SignedDuration::from_mins(2),
    }
}

/// An `ApiState` that accepts assertions from `key`.
fn state(harness: &TestDatabase, key: Option<&Ed25519KeyPair>) -> ApiState {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.assertion_key = key.map(|key| key.public_key().as_ref().to_vec());
    state
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

fn exchange(token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/sessions/telegram")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "assertion": token }).to_string(),
        ))
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

/// X-1. The credential that comes back authenticates, which is the only thing that makes the
/// exchange worth anything.
#[tokio::test]
async fn the_minted_credential_authenticates() {
    let harness = TestDatabase::create().await.expect("a test database");
    let key = issuer();
    let app = app(state(&harness, Some(&key)));
    let token = assertion::sign(&claims("100200300"), &key).expect("signing");

    let (status, body) = send(&app, exchange(&token)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let credential = body["credential"].as_str().expect("a credential");

    // The proof: use it on a route that authenticates.
    let request = Request::builder()
        .method("GET")
        .uri("/v1/capabilities")
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a request");
    let (status, capabilities) = send(&app, request).await;

    assert_eq!(status, StatusCode::OK, "{capabilities}");
    let names: Vec<&str> = capabilities["capabilities"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        names.contains(&Capability::TelegramMiniApp.as_str()),
        "a deployment that just minted a session this way advertises it: {capabilities}"
    );
}

/// X-2. An assertion is single-use, and the second presentation mints nothing.
#[tokio::test]
async fn an_assertion_mints_one_session_however_often_it_is_presented() {
    let harness = TestDatabase::create().await.expect("a test database");
    let key = issuer();
    let app = app(state(&harness, Some(&key)));
    let token = assertion::sign(&claims("100200300"), &key).expect("signing");

    let (first, _) = send(&app, exchange(&token)).await;
    let (second, body) = send(&app, exchange(&token)).await;

    assert_eq!(first, StatusCode::CREATED);
    assert_eq!(second, StatusCode::UNAUTHORIZED, "{body}");

    let sessions: i64 = sqlx::query_scalar("select count(*) from identity.sessions")
        .fetch_one(harness.pool())
        .await
        .expect("a count");
    assert_eq!(sessions, 1, "a replay must not mint a second session");
}

/// X-3. Every way of not being believed is the same refusal.
///
/// A caller must not be able to tell an expired assertion from a forged one: the difference is a
/// fact about our verification, and probing it is how an attacker learns which half to fix.
#[tokio::test]
async fn every_refusal_is_indistinguishable() {
    let harness = TestDatabase::create().await.expect("a test database");
    let key = issuer();
    let other = issuer();
    let app = app(state(&harness, Some(&key)));

    let mut expired = claims("1");
    expired.issued_at = now() - jiff::SignedDuration::from_mins(9);
    expired.expires_at = now() - jiff::SignedDuration::from_mins(8);
    let mut wrong_audience = claims("1");
    wrong_audience.audience = "ingest".to_owned();

    let attempts = [
        ("garbage", "not-a-token".to_owned()),
        (
            "another issuer's key",
            assertion::sign(&claims("1"), &other).expect("signing"),
        ),
        ("expired", assertion::sign(&expired, &key).expect("signing")),
        (
            "wrong audience",
            assertion::sign(&wrong_audience, &key).expect("signing"),
        ),
    ];

    let mut bodies = Vec::new();
    for (what, token) in attempts {
        let (status, body) = send(&app, exchange(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{what}: {body}");
        assert_eq!(body["code"], "platform.auth.unauthenticated", "{what}");
        bodies.push(body["message"].clone());
    }
    let first = &bodies[0];
    assert!(
        bodies.iter().all(|message| message == first),
        "the messages differ, so the refusals are distinguishable: {bodies:?}"
    );
}

/// X-4. A deployment with no key refuses everything, and says so through the capability document
/// rather than only through a failed attempt.
#[tokio::test]
async fn without_a_key_the_route_refuses_and_the_capability_is_absent() {
    let harness = TestDatabase::create().await.expect("a test database");
    let key = issuer();
    let app = app(state(&harness, None));
    let token = assertion::sign(&claims("1"), &key).expect("signing");

    let (status, _) = send(&app, exchange(&token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Seeded through the OTHER path so the capability can be read at all.
    let user = platform_identity::user::create_user(harness.pool(), now())
        .await
        .expect("a user");
    let credential = "capability-probe-credential-00000";
    platform_identity::session::create_session(
        harness.pool(),
        &platform_identity::NewSession {
            user_id: user.user_id,
            kind: platform_identity::SessionKind::Browser,
            device_id: None,
            audience: AUDIENCE,
            token: Some(platform_public_api::auth::credential_digest(credential)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");

    let request = Request::builder()
        .method("GET")
        .uri("/v1/capabilities")
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a request");
    let (_, capabilities) = send(&app, request).await;
    let names: Vec<&str> = capabilities["capabilities"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        !names.contains(&Capability::TelegramMiniApp.as_str()),
        "a deployment that cannot verify an assertion must not advertise the exchange: {capabilities}"
    );
}

/// X-5. One Telegram account is one internal user, however many times it signs in.
#[tokio::test]
async fn one_provider_account_is_one_internal_user() {
    let harness = TestDatabase::create().await.expect("a test database");
    let key = issuer();
    let app = app(state(&harness, Some(&key)));

    let (_, first) = send(
        &app,
        exchange(&assertion::sign(&claims("555"), &key).expect("signing")),
    )
    .await;
    let (_, second) = send(
        &app,
        exchange(&assertion::sign(&claims("555"), &key).expect("signing")),
    )
    .await;

    assert_eq!(
        first["user_id"], second["user_id"],
        "a second sign-in must not create a second person"
    );
    let users: i64 = sqlx::query_scalar("select count(*) from identity.users")
        .fetch_one(harness.pool())
        .await
        .expect("a count");
    assert_eq!(users, 1);
}

/// X-6. Both the grant and the denial are audited.
///
/// `identity.audit_events` has existed since milestone 2 with nothing writing to it. An
/// authentication decision is the case it was built for, and a denial with no trace is the half that
/// matters most.
#[tokio::test]
async fn both_outcomes_are_audited() {
    let harness = TestDatabase::create().await.expect("a test database");
    let key = issuer();
    let app = app(state(&harness, Some(&key)));

    send(
        &app,
        exchange(&assertion::sign(&claims("777"), &key).expect("signing")),
    )
    .await;
    send(&app, exchange("not-a-token")).await;

    let rows: Vec<(String, Option<Uuid>)> = sqlx::query_as(
        "select outcome, actor_user_id from identity.audit_events
          where action = 'session.exchange_assertion' order by occurred_at",
    )
    .fetch_all(harness.pool())
    .await
    .expect("the audit trail");

    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "allowed");
    assert!(rows[0].1.is_some(), "an allowed exchange names the person");
    assert_eq!(rows[1].0, "denied");
    assert!(
        rows[1].1.is_none(),
        "a denied exchange has no actor, which is exactly why it is worth recording"
    );
}

/// An Edge with the approved sites `site-a` and `site-b` of the owner, and the stub sites service.
async fn sites_edge(harness: &TestDatabase) -> Router {
    use site_owner_support as support;
    let stub = support::Stub::start().await;
    let owner = support::primary(harness.pool(), support::OWNER, true).await;
    for (site, hostname) in [
        (support::SITE, support::HOSTNAME),
        (support::OTHER_SITE, support::OTHER_HOSTNAME),
    ] {
        stub.manifest(site, hostname, &["pages.read", "notes.edit"]);
        support::approval(
            harness.pool(),
            owner,
            site,
            hostname,
            &["pages.read"],
            "approved",
        )
        .await;
    }
    support::app(support::state(harness, stub.address, &[]))
}

/// Whether the cookie `cookie` authenticates a site owner session of `site` now.
async fn site_session_live(pool: &sqlx::PgPool, cookie: &str, site: &str) -> bool {
    platform_identity::session::authenticate(
        pool,
        platform_identity::SecretDigest::of(cookie),
        &platform_identity::session::site_audience(site),
        now(),
    )
    .await
    .expect("authentication runs")
    .is_some()
}

/// Spec `site-owner-session`, "The session list": a live `site_owner` session is listed with its
/// site id, hostname, issue time and last use, and with no credential data.
#[tokio::test]
async fn site_owner_sessions_are_listed_without_credentials() {
    use site_owner_support as support;
    let harness = TestDatabase::create().await.expect("a test database");
    let app = sites_edge(&harness).await;
    let cookie = support::sign_in(&app, support::SITE, support::HOSTNAME).await;

    let request = Request::builder()
        .uri("/v1/sessions")
        .header("authorization", format!("Bearer {}", support::OWNER))
        .body(Body::empty())
        .expect("a request");
    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let site_sessions: Vec<&serde_json::Value> = body["sessions"]
        .as_array()
        .expect("a list")
        .iter()
        .filter(|session| session["kind"] == "site_owner")
        .collect();
    assert_eq!(site_sessions.len(), 1, "one site_owner entry: {body}");
    let listed = site_sessions[0];
    assert_eq!(listed["site_id"], support::SITE, "{listed}");
    assert_eq!(listed["hostname"], support::HOSTNAME, "{listed}");
    assert!(listed["issued_at"].is_string(), "{listed}");
    assert!(listed["last_seen_at"].is_string(), "{listed}");
    let text = body.to_string();
    let digest = platform_identity::SecretDigest::of(&cookie)
        .as_bytes()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    assert!(!text.contains(&cookie), "the list holds the cookie value");
    assert!(
        !text.contains(&digest),
        "the list holds the credential digest"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "The owner revokes all sessions of a site".
#[tokio::test]
async fn revoke_all_for_one_site_spares_other_sites() {
    use site_owner_support as support;
    let harness = TestDatabase::create().await.expect("a test database");
    let app = sites_edge(&harness).await;
    let first = support::sign_in(&app, support::SITE, support::HOSTNAME).await;
    let second = support::sign_in(&app, support::SITE, support::HOSTNAME).await;
    let other = support::sign_in(&app, support::OTHER_SITE, support::OTHER_HOSTNAME).await;

    let request = Request::builder()
        .method("POST")
        .uri("/v1/sessions/revoke-site")
        .header("authorization", format!("Bearer {}", support::OWNER))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"site_id":"site-a"}"#))
        .expect("a request");
    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revoked"], 2, "{body}");
    let pool = harness.pool();
    assert!(
        !site_session_live(pool, &first, support::SITE).await,
        "the first session of site-a lives"
    );
    assert!(
        !site_session_live(pool, &second, support::SITE).await,
        "the second session of site-a lives"
    );
    assert!(
        site_session_live(pool, &other, support::OTHER_SITE).await,
        "the session of site-b ended"
    );

    harness.cleanup().await.expect("cleanup");
}
