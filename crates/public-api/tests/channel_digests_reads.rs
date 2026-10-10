//! The channel-digest read routes (XR-021 CONTRACTS.md S08): Platform authenticates the caller,
//! then asks the digest service on its loopback port with the service bearer and the owner header,
//! and relays the typed answer.
//!
//! The digest service is a stub that checks the credentials it is given, so a request that carried
//! the caller's own session token, or no owner, or the wrong owner, is a failed test and not a pass.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse as _;
use axum::routing::get;
use axum::{Json, Router};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::PublicConfig;
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::channel_digests::ChannelDigestsClient;
use platform_public_api::{ApiState, auth};
use secrecy::SecretString;
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "channel-digest-reads-credential-0000";
const AUDIENCE: &str = "edge";
const SERVICE_SECRET: &str = "digest-service-secret-0123456789";

/// A result id the stub answers 404 for.
const MISSING: &str = "00000000-0000-4000-8000-000000000404";
/// A result id the stub answers 500 for.
const BROKEN: &str = "00000000-0000-4000-8000-000000000500";
/// A result id the stub answers with a body that is not JSON.
const NOT_JSON: &str = "00000000-0000-4000-8000-0000000000aa";
/// A result id the stub answers with a body above the 262144-byte cap.
const OVERSIZED: &str = "00000000-0000-4000-8000-0000000000bb";
/// A result id the stub answers with JSON that is not a result view.
const WRONG_SHAPE: &str = "00000000-0000-4000-8000-0000000000cc";
/// A result the stub knows.
const KNOWN: &str = "018f0000-0000-7000-8000-000000000003";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// One request the digest stub accepted: its path and its query parameters.
type Recorded = (String, BTreeMap<String, String>);

#[derive(Debug, Clone, Default)]
struct Seen {
    requests: Arc<Mutex<Vec<Recorded>>>,
}

#[derive(Clone)]
struct Stub {
    seen: Seen,
    owner: Uuid,
}

impl Stub {
    /// Refuse anything that is not the service bearer plus the expected owner header.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(&format!("Bearer {SERVICE_SECRET}"))
            && headers
                .get("x-ratatoskr-owner-id")
                .and_then(|value| value.to_str().ok())
                == Some(&self.owner.to_string())
    }

    fn record(&self, path: &str, query: BTreeMap<String, String>) {
        self.seen
            .requests
            .lock()
            .expect("an uncontended recorder")
            .push((path.to_owned(), query));
    }
}

async fn subscriptions(
    State(stub): State<Stub>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> axum::response::Response {
    if !stub.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    stub.record("/v1/subscriptions", query);
    Json(serde_json::json!({
        "subscriptions": [{
            "subscription_id": "018f0000-0000-7000-8000-000000000001",
            "channel_username": "example_channel",
            "enabled": true
        }]
    }))
    .into_response()
}

async fn results(
    State(stub): State<Stub>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> axum::response::Response {
    if !stub.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    stub.record("/v1/results", query);
    Json(serde_json::json!({
        "results": [{
            "result_id": KNOWN,
            "run_id": "018f0000-0000-7000-8000-000000000002",
            "outcome": "failed",
            "safe_failure_class": "provider_unavailable",
            "created_at": "2026-10-10T06:00:00Z"
        }]
    }))
    .into_response()
}

async fn result(
    State(stub): State<Stub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !stub.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    stub.record(&format!("/v1/results/{id}"), BTreeMap::new());
    match id.as_str() {
        MISSING => StatusCode::NOT_FOUND.into_response(),
        BROKEN => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        NOT_JSON => "this is not json".into_response(),
        OVERSIZED => "a".repeat(262_145).into_response(),
        WRONG_SHAPE => Json(serde_json::json!({ "unexpected": true })).into_response(),
        _ => Json(serde_json::json!({
            "result_id": id,
            "run_id": "018f0000-0000-7000-8000-000000000002",
            "outcome": "completed",
            "recap_id": "018f0000-0000-7000-8000-000000000004",
            "citation_count": 3,
            "result_digest": {
                "algorithm": "sha256",
                "hex": "ab".repeat(32)
            },
            "recap": { "headline": "A headline", "topics": ["one", "two"] }
        }))
        .into_response(),
    }
}

async fn start_stub(owner: Uuid) -> (std::net::SocketAddr, Seen, tokio::task::JoinHandle<()>) {
    let seen = Seen::default();
    let router = Router::new()
        .route("/v1/subscriptions", get(subscriptions))
        .route("/v1/results", get(results))
        .route("/v1/results/{result_id}", get(result))
        .with_state(Stub {
            seen: seen.clone(),
            owner,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the digest stub serves");
    });
    (address, seen, task)
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

fn state_with(harness: &TestDatabase, address: Option<std::net::SocketAddr>) -> ApiState {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.channel_digests = address.map(|address| {
        Arc::new(ChannelDigestsClient::new(
            address,
            SecretString::from(SERVICE_SECRET),
        ))
    });
    state
}

async fn get_path(app: &Router, path: &str) -> (StatusCode, HeaderMap, serde_json::Value) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .body(Body::empty())
        .expect("a request");
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

#[tokio::test]
async fn results_get_forwards_bearer_and_owner_id_and_returns_the_contract_view() {
    let harness = TestDatabase::create().await.expect("a test database");
    let user = seed(harness.pool()).await;
    let (address, seen, task) = start_stub(user).await;
    let app = app(state_with(&harness, Some(address)));

    let (status, headers, view) =
        get_path(&app, &format!("/v1/channel-digests/results/{KNOWN}")).await;

    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(view["result_id"], KNOWN);
    assert_eq!(view["outcome"], "completed");
    assert_eq!(view["citation_count"], 3);
    assert_eq!(view["recap"]["headline"], "A headline");
    assert!(
        view.get("safe_failure_class").is_none(),
        "absent optionals are not serialized: {view}"
    );
    assert_eq!(
        seen.requests.lock().expect("a recorder").len(),
        1,
        "the stub saw one authorized request"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn upstream_404_is_404() {
    let harness = TestDatabase::create().await.expect("a test database");
    let user = seed(harness.pool()).await;
    let (address, _, task) = start_stub(user).await;
    let app = app(state_with(&harness, Some(address)));

    let (status, _, body) = get_path(&app, &format!("/v1/channel-digests/results/{MISSING}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "platform.resource.not_found");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// CONTRACTS.md S08: only a 404 keeps its meaning; any other non-2xx answer is an invalid upstream
/// response (502), and `UpstreamUnavailable` is for a transport failure.
#[tokio::test]
async fn upstream_500_is_upstream_invalid_response() {
    let harness = TestDatabase::create().await.expect("a test database");
    let user = seed(harness.pool()).await;
    let (address, _, task) = start_stub(user).await;
    let app = app(state_with(&harness, Some(address)));

    let (status, _, body) = get_path(&app, &format!("/v1/channel-digests/results/{BROKEN}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "edge.upstream_invalid_response");
    task.abort();

    // A listener nobody serves is the transport failure.
    let app = app_for_dead_listener(&harness).await;
    let (status, _, body) = get_path(&app, &format!("/v1/channel-digests/results/{KNOWN}")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "edge.upstream_unavailable");
    harness.cleanup().await.expect("cleanup");
}

async fn app_for_dead_listener(harness: &TestDatabase) -> Router {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    drop(listener);
    app(state_with(harness, Some(address)))
}

#[tokio::test]
async fn oversized_or_invalid_json_is_upstream_invalid_response() {
    let harness = TestDatabase::create().await.expect("a test database");
    let user = seed(harness.pool()).await;
    let (address, _, task) = start_stub(user).await;
    let app = app(state_with(&harness, Some(address)));

    for id in [NOT_JSON, OVERSIZED, WRONG_SHAPE] {
        let (status, _, body) = get_path(&app, &format!("/v1/channel-digests/results/{id}")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{id}: {body}");
        assert_eq!(body["code"], "edge.upstream_invalid_response", "{id}");
    }
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn absent_config_is_upstream_unavailable_and_commands_still_work() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let app = app(state_with(&harness, None));

    for path in [
        "/v1/channel-digests/subscriptions".to_owned(),
        "/v1/channel-digests/results".to_owned(),
        format!("/v1/channel-digests/results/{KNOWN}"),
    ] {
        let (status, _, body) = get_path(&app, &path).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
        assert_eq!(body["code"], "edge.upstream_unavailable");
    }

    let command = Request::builder()
        .method("POST")
        .uri("/v1/channel-digests/runs")
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("idempotency-key", "run-without-reads")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"output_language":"en"}"#))
        .expect("a request");
    let response = app.clone().oneshot(command).await.expect("a response");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn list_routes_pass_page_size_through() {
    let harness = TestDatabase::create().await.expect("a test database");
    let user = seed(harness.pool()).await;
    let (address, seen, task) = start_stub(user).await;
    let app = app(state_with(&harness, Some(address)));

    let (status, _, page) = get_path(&app, "/v1/channel-digests/subscriptions?page_size=7").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["subscriptions"][0]["channel_username"],
        "example_channel"
    );
    assert_eq!(page["subscriptions"][0]["enabled"], true);
    let (status, _, page) = get_path(&app, "/v1/channel-digests/results?page_size=100").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["results"][0]["outcome"], "failed");
    assert_eq!(
        page["results"][0]["safe_failure_class"],
        "provider_unavailable"
    );

    for path in [
        "/v1/channel-digests/subscriptions?page_size=0",
        "/v1/channel-digests/results?page_size=101",
        "/v1/channel-digests/results?page_size=many",
        "/v1/channel-digests/results?limit=5",
    ] {
        let (status, _, body) = get_path(&app, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }

    let recorded = seen.requests.lock().expect("a recorder").clone();
    assert_eq!(
        recorded,
        vec![
            (
                "/v1/subscriptions".to_owned(),
                BTreeMap::from([("page_size".to_owned(), "7".to_owned())])
            ),
            (
                "/v1/results".to_owned(),
                BTreeMap::from([("page_size".to_owned(), "100".to_owned())])
            ),
        ],
        "only the two valid requests reached the digest service"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}
