//! The provider callback facade — tests C-1 … C-7.
//!
//! ADR-0012 makes two promises that a reviewer cannot check by reading: the authorization code
//! reaches the owning service and nowhere else, and it can be taken exactly once. Both are asserted
//! here against the database rather than against the handler.

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
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tower::ServiceExt as _;
use uuid::Uuid;

/// The capability a caller must hold to claim a GitHub callback, per `oauth::claim_grant`.
const CLAIM: &str = "oauth.claim.github";
/// The listener's audience. EVERY session presented here has it, service or person — which is
/// exactly why a relay cannot be bound to it and is bound to a grant instead.
const AUDIENCE: &str = "edge";
const CODE: &str = "provider-authorization-code-9f2a";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

/// The edge listener, as it actually runs: one audience for everybody.
fn state(harness: &TestDatabase) -> ApiState {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.oauth_completion_url = Some("https://ratatoskr.test/done".parse().expect("a url"));
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

/// A caller: a session of `kind` on the edge listener, holding `grants`.
async fn caller(pool: &sqlx::PgPool, kind: SessionKind, grants: &[&str]) -> String {
    let credential = format!("caller-credential-{}", Uuid::now_v7().simple());
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    for capability in grants {
        platform_identity::grant::grant(pool, user.user_id, capability, now(), None)
            .await
            .expect("a grant");
    }
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind,
            device_id: None,
            audience: AUDIENCE,
            token: Some(auth::credential_digest(&credential)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    credential
}

async fn send(
    app: &Router,
    request: Request<Body>,
) -> (StatusCode, Option<String>, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let location = response
        .headers()
        .get(http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, location, json)
}

fn callback(provider: &str, query: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v2/oauth/{provider}/callback?{query}"))
        .body(Body::empty())
        .expect("a request")
}

fn claim(relay_id: Uuid, credential: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v2/oauth/relays/{relay_id}"))
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a request")
}

async fn only_relay(pool: &sqlx::PgPool) -> Uuid {
    sqlx::query_scalar("select relay_id from identity.oauth_relays")
        .fetch_one(pool)
        .await
        .expect("exactly one relay")
}

/// C-1. A callback is recorded once and the browser is sent to the CONFIGURED page.
#[tokio::test]
async fn a_callback_is_recorded_and_the_browser_is_sent_to_the_configured_page() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(state(&harness));

    let (status, location, _) = send(
        &app,
        callback("github", &format!("state=abc123&code={CODE}")),
    )
    .await;

    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("https://ratatoskr.test/done"));

    let (provider, required, state_value): (String, String, String) =
        sqlx::query_as("select provider, claim_grant, state from identity.oauth_relays")
            .fetch_one(harness.pool())
            .await
            .expect("one relay");
    assert_eq!(provider, "github");
    assert_eq!(
        required, CLAIM,
        "claimable only by a holder of the provider's capability"
    );
    assert_eq!(state_value, "abc123", "carried verbatim");
}

/// C-2. The code is claimable exactly once, and the claim destroys it.
#[tokio::test]
async fn the_code_is_returned_once_and_then_no_longer_exists() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(state(&harness));
    let credential = caller(harness.pool(), SessionKind::Service, &[CLAIM]).await;
    send(&app, callback("github", &format!("state=s&code={CODE}"))).await;
    let relay_id = only_relay(harness.pool()).await;

    let (first, _, body) = send(&app, claim(relay_id, &credential)).await;
    assert_eq!(first, StatusCode::OK, "{body}");
    assert_eq!(body["code"], CODE);
    assert_eq!(body["state"], "s");

    let (second, _, _) = send(&app, claim(relay_id, &credential)).await;
    assert_eq!(second, StatusCode::NOT_FOUND, "a claim is single-use");

    let stored: Option<String> =
        sqlx::query_scalar("select code from identity.oauth_relays where relay_id = $1")
            .bind(relay_id)
            .fetch_one(harness.pool())
            .await
            .expect("the row");
    assert!(
        stored.is_none(),
        "the claim must destroy the code, leaving the row as evidence the callback arrived"
    );
}

/// C-3. The code reaches the owning service and NOTHING else — no command, no outbox row.
///
/// The promise ADR-0012 makes that a reader cannot check: option (a) it rejected would have put the
/// code in `operations.outbox.payload` and then in a `JetStream` file store, two durable copies of a
/// live credential in the two places an operator pages through while debugging.
#[tokio::test]
async fn the_code_appears_in_no_command_and_no_outbox_payload() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(state(&harness));

    send(&app, callback("github", &format!("state=s&code={CODE}"))).await;

    let payloads: Vec<String> = sqlx::query_scalar("select payload::text from operations.outbox")
        .fetch_all(harness.pool())
        .await
        .expect("the outbox");
    assert!(
        payloads.iter().all(|payload| !payload.contains(CODE)),
        "the authorization code reached the outbox: {payloads:?}"
    );
}

/// C-4. Four ways of not being allowed to claim, one answer.
///
/// Which relays exist and which service each is for is not a caller's business, so an unknown
/// relay, another service's relay, a person's session and an expired relay must be
/// indistinguishable (`ARCHITECTURE.md` S15).
#[tokio::test]
async fn every_refused_claim_is_the_same_answer() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner_app = app(state(&harness));
    send(
        &owner_app,
        callback("github", &format!("state=s&code={CODE}")),
    )
    .await;
    let relay_id = only_relay(pool).await;

    // A person's session with the owning audience.
    let person = caller(pool, SessionKind::Browser, &[CLAIM]).await;
    // A service that holds a DIFFERENT provider's claim capability.
    let other_service = caller(pool, SessionKind::Service, &["oauth.claim.telegram"]).await;
    let owner = caller(pool, SessionKind::Service, &[CLAIM]).await;

    let attempts = [
        (
            "an unknown relay",
            &owner_app,
            Uuid::now_v7(),
            owner.clone(),
        ),
        ("a person's session", &owner_app, relay_id, person),
        (
            "another provider's service",
            &owner_app,
            relay_id,
            other_service,
        ),
    ];
    for (what, app, relay, credential) in attempts {
        let (status, _, body) = send(app, claim(relay, &credential)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{what}: {body}");
        assert_eq!(body["code"], "platform.resource.not_found", "{what}");
    }

    // And an expired one, aged in place.
    // Both instants move: the schema requires `expires_at > received_at`, so ageing only the
    // expiry would test the CHECK rather than the claim.
    sqlx::query(
        "update identity.oauth_relays
            set received_at = now() - interval '10 minutes',
                expires_at  = now() - interval '5 minutes'",
    )
    .execute(pool)
    .await
    .expect("ageing the relay");
    let (status, _, body) = send(&owner_app, claim(relay_id, &owner)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an expired relay: {body}");
}

/// C-5. The route refuses what it cannot record, before it records anything.
#[tokio::test]
async fn a_callback_that_says_nothing_usable_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(state(&harness));

    let refused = [
        (
            "no provider we serve",
            "weather",
            "state=s&code=c",
            StatusCode::NOT_FOUND,
        ),
        ("no state", "github", "code=c", StatusCode::BAD_REQUEST),
        (
            "neither code nor error",
            "github",
            "state=s",
            StatusCode::BAD_REQUEST,
        ),
        (
            "both code and error",
            "github",
            "state=s&code=c&error=denied",
            StatusCode::BAD_REQUEST,
        ),
        (
            "an oversized code",
            "github",
            &format!("state=s&code={}", "x".repeat(2049)),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (what, provider, query, expected) in refused {
        let (status, _, body) = send(&app, callback(provider, query)).await;
        assert_eq!(status, expected, "{what}: {body}");
    }

    let relays: i64 = sqlx::query_scalar("select count(*) from identity.oauth_relays")
        .fetch_one(harness.pool())
        .await
        .expect("a count");
    assert_eq!(relays, 0, "a refused callback records nothing");
}

/// C-6. A provider error is relayed rather than dropped, so the owning service can end its own flow
/// instead of waiting for a claim that never comes.
#[tokio::test]
async fn a_refusal_by_the_person_is_relayed_too() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(state(&harness));
    let credential = caller(harness.pool(), SessionKind::Service, &[CLAIM]).await;

    let (status, _, _) = send(&app, callback("github", "state=s&error=access_denied")).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the person is finished either way"
    );

    let relay_id = only_relay(harness.pool()).await;
    let (status, _, body) = send(&app, claim(relay_id, &credential)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"], "access_denied");
    assert!(body["code"].is_null());
}

/// C-7. Both the callback and the claim are audited, including the claim that was refused.
#[tokio::test]
async fn the_callback_and_every_claim_are_audited() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(state(&harness));
    let credential = caller(harness.pool(), SessionKind::Service, &[CLAIM]).await;
    send(&app, callback("github", &format!("state=s&code={CODE}"))).await;
    let relay_id = only_relay(harness.pool()).await;

    send(&app, claim(relay_id, &credential)).await;
    send(&app, claim(relay_id, &credential)).await;

    let rows: Vec<(String, String)> =
        sqlx::query_as("select action, outcome from identity.audit_events order by occurred_at")
            .fetch_all(harness.pool())
            .await
            .expect("the audit trail");

    assert_eq!(
        rows,
        vec![
            ("oauth.callback".to_owned(), "allowed".to_owned()),
            ("oauth.relay_claim".to_owned(), "allowed".to_owned()),
            ("oauth.relay_claim".to_owned(), "denied".to_owned()),
        ]
    );
}
