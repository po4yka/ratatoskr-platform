//! The build request of a site release (workspace change `add-site-publications`, specs
//! `site-route-classes` and `site-publications`): Edge serves `POST /v1/sites/{site}/releases` in
//! the class `release`, mints an operation of the kind `sites.release.build` and writes
//! `cmd.sites.release.build_requested.v1` to its outbox.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::routing::any;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const AUDIENCE: &str = "edge";
const OWNER: &str = "sites-build-owner-0000000000000000";
const MEMBER: &str = "sites-build-member-000000000000000";
const SUBJECT: &str = "cmd.sites.release.build_requested.v1";
const RELEASES: &str = "/v1/sites/demo-site/releases";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// A primary browser session, with the live owner grant when `owner`. Returns the user id.
async fn seed(pool: &sqlx::PgPool, credential: &str, owner: bool) -> uuid::Uuid {
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
    if owner {
        platform_identity::grant::grant(pool, user.user_id, PLATFORM_OWNER_GRANT, now(), None)
            .await
            .expect("the owner grant");
    }
    user.user_id
}

/// A stub sites service that counts the requests it receives. Edge serves the build route
/// itself, so the stub must receive none.
async fn stub_sites() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let router = Router::new().route(
        "/v1/sites/{*rest}",
        any(move || {
            let seen = Arc::clone(&seen);
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the stub serves");
    });
    (address, count)
}

fn app(harness: &TestDatabase, listener: std::net::SocketAddr) -> Router {
    let gateway = GatewayConfig {
        routes: BTreeMap::from([(
            "sites".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/sites".to_owned(),
                listener,
                class: Some(GatewayRouteClass::Control),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        )]),
        ..GatewayConfig::default()
    };
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(&gateway);
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 30,
        max_body_bytes: 1_048_576,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 1200,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(Arc::new(state)),
    )
}

async fn request_build(app: &Router, credential: Option<&str>, key: &str) -> (u16, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(RELEASES)
        .header("idempotency-key", key);
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("a response");
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The envelope of every build command in the outbox.
async fn commands(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "select payload from operations.outbox where subject = $1 order by enqueued_at",
    )
    .bind(SUBJECT)
    .fetch_all(pool)
    .await
    .expect("the outbox")
}

/// Spec `site-route-classes`, "The owner requests a build".
#[tokio::test]
async fn owner_build_request_publishes_one_command() {
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed(harness.pool(), OWNER, true).await;
    let (listener, count) = stub_sites().await;
    let app = app(&harness, listener);

    let (status, body) = request_build(&app, Some(OWNER), "build-1").await;
    let written = commands(harness.pool()).await;
    assert_eq!(
        written.len(),
        1,
        "one build command in the outbox (answer {status}: {body})"
    );
    assert_eq!(status, 202, "{body}");
    let operation_id = body["operation_id"].as_str().expect("the operation id");
    let command = &written[0];
    assert_eq!(command["command_type"], "sites.release.build_requested.v1");
    assert_eq!(command["producer"], "ratatoskr-platform");
    assert_eq!(command["aggregate_id"], "site:demo-site");
    assert_eq!(command["tenant_id"], format!("user:{owner}"));
    assert_eq!(
        command["payload"],
        serde_json::json!({ "operation_id": operation_id, "site_id": "demo-site" })
    );
    let kind: String =
        sqlx::query_scalar("select kind from operations.operations where operation_id = $1::uuid")
            .bind(operation_id)
            .fetch_one(harness.pool())
            .await
            .expect("the operation");
    assert_eq!(kind, "sites.release.build");

    let (status, replay) = request_build(&app, Some(OWNER), "build-1").await;
    assert_eq!(status, 202);
    assert_eq!(replay["operation_id"], body["operation_id"], "a replay");
    assert_eq!(
        commands(harness.pool()).await.len(),
        1,
        "a replay writes nothing"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "Edge serves the build route; the sites service receives nothing"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-route-classes`, "An anonymous build request".
#[tokio::test]
async fn anonymous_build_request_writes_no_command() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), MEMBER, false).await;
    let (listener, count) = stub_sites().await;
    let app = app(&harness, listener);

    let (status, body) = request_build(&app, None, "anonymous").await;
    assert_eq!(
        (status, body["code"].as_str()),
        (401, Some("platform.auth.unauthenticated"))
    );
    let (status, body) = request_build(&app, Some(MEMBER), "member").await;
    assert_eq!(
        (status, body["code"].as_str()),
        (403, Some("platform.auth.forbidden")),
        "a session without the owner grant"
    );
    assert!(commands(harness.pool()).await.is_empty(), "no command");
    let operations: i64 = sqlx::query_scalar("select count(*) from operations.operations")
        .fetch_one(harness.pool())
        .await
        .expect("the operations");
    assert_eq!(operations, 0, "no operation");
    assert_eq!(count.load(Ordering::SeqCst), 0);

    harness.cleanup().await.expect("cleanup");
}
