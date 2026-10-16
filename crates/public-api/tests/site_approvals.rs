//! The site approval of Platform (workspace change `add-site-publications`, spec `site-manifest`):
//! Edge serves `POST` and `DELETE /v1/sites/{site}/manifest/approval`, reads the active manifest
//! from the internal route of a stub sites service, stores the approval in
//! `identity.site_approvals` and writes `evt.platform.site_approval.changed.v1` to the outbox.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::response::IntoResponse as _;
use axum::routing::get;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;
use serde_json::{Value, json};
use sqlx::Row as _;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const AUDIENCE: &str = "edge";
const OWNER: &str = "site-approvals-owner-000000000000";
const SUBJECT: &str = "evt.platform.site_approval.changed.v1";
const FIRST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const SECOND: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// The owner: a primary session with the live owner grant. Returns the user id.
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
            token: Some(auth::credential_digest(OWNER)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    platform_identity::grant::grant(pool, user.user_id, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("the owner grant");
    user.user_id
}

/// The active manifests of the stub sites service, by site id.
type Manifests = Arc<Mutex<BTreeMap<String, Value>>>;

fn manifest(site: &str, digest: &str, hostname: &str) -> Value {
    json!({
        "site_id": site,
        "manifest_sha256": digest,
        "hostname": hostname,
        "publications": ["garden"],
        "capabilities": ["pages.read", "assets.read"],
        "github_repository": {"repository": "owner/docs-site", "repository_id": "12345"},
    })
}

/// A stub sites service that serves only its internal manifest route. Any other path is a contract
/// `404`, as the sites service answers an unknown route.
async fn stub_sites(manifests: Manifests) -> std::net::SocketAddr {
    let router = Router::new()
        .route(
            "/internal/sites/{site}/manifest",
            get(move |Path(site): Path<String>| {
                let manifests = Arc::clone(&manifests);
                async move {
                    let active = manifests.lock().expect("the stub map").get(&site).cloned();
                    match active {
                        Some(active) => Json(active).into_response(),
                        None => not_found().into_response(),
                    }
                }
            }),
        )
        .fallback(|| async { not_found() });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the stub serves");
    });
    address
}

fn not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "code": "sites.route_not_found",
            "message": "No such route.",
            "retryable": false,
        })),
    )
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

async fn call(app: &Router, method: &str, site: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(format!("/v1/sites/{site}/manifest/approval"))
        .header("authorization", format!("Bearer {OWNER}"))
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("a request");
    let response = app.clone().oneshot(request).await.expect("a response");
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

async fn approve(app: &Router, site: &str, digest: &str) -> (u16, Value) {
    call(
        app,
        "POST",
        site,
        Some(json!({ "manifest_sha256": digest })),
    )
    .await
}

/// `(site_id, manifest_sha256, capabilities, hostname, state)` of every stored approval.
async fn approvals(pool: &sqlx::PgPool) -> Vec<(String, String, Vec<String>, String, String)> {
    sqlx::query(
        "select site_id, manifest_sha256, capabilities, hostname, state
           from identity.site_approvals order by site_id",
    )
    .fetch_all(pool)
    .await
    .expect("the approvals")
    .into_iter()
    .map(|row| {
        (
            row.get("site_id"),
            row.get("manifest_sha256"),
            row.get("capabilities"),
            row.get("hostname"),
            row.get("state"),
        )
    })
    .collect()
}

/// The envelope of every approval event in the outbox, in the order they were written.
async fn announced(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "select payload from operations.outbox where subject = $1 order by enqueued_at, outbox_id",
    )
    .bind(SUBJECT)
    .fetch_all(pool)
    .await
    .expect("the outbox")
}

/// SITE-028: the authoritative internal manifest supplies the approved immutable identity.
#[tokio::test]
async fn approval_event_covers_the_repository() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let manifests = Manifests::default();
    let binding = json!({"repository": "owner/docs-site", "repository_id": "12345"});
    let mut active = manifest("demo-site", FIRST, "garden.example.org");
    active["github_repository"] = binding.clone();
    manifests
        .lock()
        .unwrap()
        .insert("demo-site".to_owned(), active);
    let app = app(&harness, stub_sites(Arc::clone(&manifests)).await);
    let (status, body) = approve(&app, "demo-site", FIRST).await;
    assert_eq!(status, 200, "{body}");
    let events = announced(harness.pool()).await;
    let result = (
        body["github_repository"].clone(),
        events[0]["payload"]["github_repository"].clone(),
    );
    let (status, withdrawn) = call(&app, "DELETE", "demo-site", None).await;
    assert_eq!(status, 200, "{withdrawn}");
    let events = announced(harness.pool()).await;
    let withdrawal = events[1]["payload"]["github_repository"].clone();
    harness.cleanup().await.expect("cleanup");
    assert_eq!(result, (binding.clone(), binding.clone()));
    assert_eq!(withdrawal, binding);
}

/// SITE-028: approving a manifest without a repository cannot create deploy authority.
#[tokio::test]
async fn new_approval_requires_repository_binding() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let manifests = Manifests::default();
    let mut active = manifest("demo-site", FIRST, "garden.example.org");
    active
        .as_object_mut()
        .expect("a manifest object")
        .remove("github_repository");
    manifests
        .lock()
        .unwrap()
        .insert("demo-site".to_owned(), active);
    let app = app(&harness, stub_sites(manifests).await);
    let (status, body) = approve(&app, "demo-site", FIRST).await;
    let rows = approvals(harness.pool()).await;
    let events = announced(harness.pool()).await;
    harness.cleanup().await.expect("cleanup");
    assert_eq!(status, 400, "an unbound manifest must be refused: {body}");
    assert!(rows.is_empty() && events.is_empty());
}

/// Spec `site-manifest`, "An approval is stored in Platform and announced once".
#[tokio::test]
async fn approval_is_stored_and_announced() {
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let manifests = Manifests::default();
    manifests.lock().unwrap().insert(
        "demo-site".to_owned(),
        manifest("demo-site", FIRST, "garden.example.org"),
    );
    let app = app(&harness, stub_sites(Arc::clone(&manifests)).await);

    let (status, body) = approve(&app, "demo-site", FIRST).await;
    assert_eq!(
        status, 200,
        "the owner approves the active manifest: {body}"
    );
    assert_eq!(
        approvals(harness.pool()).await,
        [(
            "demo-site".to_owned(),
            FIRST.to_owned(),
            vec!["pages.read".to_owned(), "assets.read".to_owned()],
            "garden.example.org".to_owned(),
            "approved".to_owned(),
        )]
    );
    let events = announced(harness.pool()).await;
    assert_eq!(events.len(), 1, "one event: {events:?}");
    let event = &events[0];
    assert_eq!(event["event_type"], "platform.site_approval.changed.v1");
    assert_eq!(event["producer"], "ratatoskr-platform");
    assert_eq!(event["aggregate_id"], "site:demo-site");
    assert_eq!(event["tenant_id"], format!("user:{owner}"));
    assert_eq!(
        event["payload"],
        json!({
            "site_id": "demo-site",
            "manifest_sha256": FIRST,
            "capabilities": ["pages.read", "assets.read"],
            "hostname": "garden.example.org",
            "state": "approved",
            "github_repository": {"repository": "owner/docs-site", "repository_id": "12345"},
        })
    );

    let (status, _) = approve(&app, "demo-site", FIRST).await;
    assert_eq!(status, 200, "a repeated approval is accepted");
    assert_eq!(
        announced(harness.pool()).await.len(),
        1,
        "a repeated approval changes nothing and announces nothing"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-manifest`, "A stale digest is refused".
#[tokio::test]
async fn stale_digest_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let manifests = Manifests::default();
    manifests.lock().unwrap().insert(
        "demo-site".to_owned(),
        manifest("demo-site", SECOND, "garden.example.org"),
    );
    let app = app(&harness, stub_sites(Arc::clone(&manifests)).await);

    let (status, body) = approve(&app, "demo-site", FIRST).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("platform.site.manifest_digest_stale")),
        "{body}"
    );
    assert!(approvals(harness.pool()).await.is_empty(), "no row changes");
    assert!(
        announced(harness.pool()).await.is_empty(),
        "nothing is announced"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-manifest`, "A second site claims a hostname".
#[tokio::test]
async fn hostname_is_unique_across_approved_sites() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let manifests = Manifests::default();
    manifests.lock().unwrap().extend([
        (
            "site-a".to_owned(),
            manifest("site-a", FIRST, "garden.example.org"),
        ),
        (
            "site-b".to_owned(),
            manifest("site-b", SECOND, "garden.example.org"),
        ),
    ]);
    let app = app(&harness, stub_sites(Arc::clone(&manifests)).await);

    assert_eq!(approve(&app, "site-a", FIRST).await.0, 200);
    let (status, body) = approve(&app, "site-b", SECOND).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("platform.site.hostname_taken")),
        "{body}"
    );
    let stored = approvals(harness.pool()).await;
    assert_eq!(stored.len(), 1, "site b has no approval: {stored:?}");
    assert_eq!(
        (stored[0].0.as_str(), stored[0].4.as_str()),
        ("site-a", "approved")
    );
    assert_eq!(announced(harness.pool()).await.len(), 1);

    assert_eq!(call(&app, "DELETE", "site-a", None).await.0, 200);
    assert_eq!(
        approve(&app, "site-b", SECOND).await.0,
        200,
        "a withdrawn approval frees its hostname"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-manifest`, "A withdrawal reaches the projection" (the Platform half): the withdrawal
/// is stored and announced, and the event repeats the approval it ends.
#[tokio::test]
async fn withdrawal_is_stored_and_announced() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed_owner(harness.pool()).await;
    let manifests = Manifests::default();
    manifests.lock().unwrap().insert(
        "demo-site".to_owned(),
        manifest("demo-site", FIRST, "garden.example.org"),
    );
    let app = app(&harness, stub_sites(Arc::clone(&manifests)).await);

    assert_eq!(approve(&app, "demo-site", FIRST).await.0, 200);
    let (status, body) = call(&app, "DELETE", "demo-site", None).await;
    assert_eq!(status, 200, "the owner withdraws the approval: {body}");
    assert_eq!(
        approvals(harness.pool()).await,
        [(
            "demo-site".to_owned(),
            FIRST.to_owned(),
            vec!["pages.read".to_owned(), "assets.read".to_owned()],
            "garden.example.org".to_owned(),
            "withdrawn".to_owned(),
        )]
    );
    let events = announced(harness.pool()).await;
    assert_eq!(events.len(), 2, "an approval and a withdrawal: {events:?}");
    assert_eq!(
        events[1]["payload"],
        json!({
            "site_id": "demo-site",
            "manifest_sha256": FIRST,
            "capabilities": ["pages.read", "assets.read"],
            "hostname": "garden.example.org",
            "state": "withdrawn",
            "github_repository": {"repository": "owner/docs-site", "repository_id": "12345"},
        })
    );

    assert_eq!(call(&app, "DELETE", "demo-site", None).await.0, 200);
    assert_eq!(
        announced(harness.pool()).await.len(),
        2,
        "a repeated withdrawal announces nothing"
    );
    assert_eq!(
        call(&app, "DELETE", "never-approved", None).await.0,
        404,
        "a site without an approval has nothing to withdraw"
    );

    harness.cleanup().await.expect("cleanup");
}
