//! Edge gateway acceptance tests.
//!
//! These tests use loopback HTTP servers rather than a mock client so they exercise the public
//! authentication extractor, route matching, streaming body forwarding and the actual hyper
//! transport Edge uses in production.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::routing::any;
use futures_util::StreamExt as _;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

const AUDIENCE: &str = "edge";
const CREDENTIAL: &str = "gateway-credential-000000000000";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

async fn seed(pool: &sqlx::PgPool) {
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
}

fn gateway(listener: std::net::SocketAddr, class: GatewayRouteClass) -> GatewayConfig {
    GatewayConfig {
        routes: BTreeMap::from([(
            "knowledge".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/k".to_owned(),
                listener,
                class: Some(class),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        )]),
        ..GatewayConfig::default()
    }
}

fn with_control_budget(
    mut gateway: GatewayConfig,
    max_body_bytes: u64,
    response_timeout_seconds: u64,
) -> GatewayConfig {
    gateway.budgets.control.max_body_bytes = max_body_bytes;
    gateway.budgets.control.response_timeout_seconds = response_timeout_seconds;
    gateway
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "test call sites construct a distinct route table for each fixture"
)]
fn app(harness: &TestDatabase, gateway: GatewayConfig, request_timeout_seconds: u64) -> Router {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(&gateway);
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds,
        max_body_bytes: 4 * 1_048_576,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(Arc::new(state)),
    )
}

async fn stub(router: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the fixture server serves");
    });
    (address, task)
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a response body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn request(credential: Option<&str>, body: Body) -> Request<Body> {
    let mut request = Request::builder().method("POST").uri("/v1/k/search");
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    request.body(body).expect("a request")
}

/// Authentication is an Edge responsibility: without a valid session no bytes reach the service.
#[tokio::test]
async fn unauthenticated_gateway_request_never_calls_downstream() {
    let (called_tx, mut called_rx) = mpsc::channel::<()>(1);
    let (address, task) = stub(Router::new().route(
        "/v1/k/search",
        any(move || {
            let called_tx = called_tx.clone();
            async move {
                called_tx.send(()).await.expect("record downstream call");
                StatusCode::OK
            }
        }),
    ))
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(&harness, gateway(address, GatewayRouteClass::Control), 15);

    let (status, body) = send(&app, request(None, Body::empty())).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "platform.auth.unauthenticated");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), called_rx.recv())
            .await
            .is_err(),
        "an unauthenticated request reached the downstream"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// A plaintext downstream error is not a public contract response and must never escape Edge.
#[tokio::test]
async fn nonconforming_downstream_error_becomes_an_edge_error_envelope() {
    let (address, task) = stub(Router::new().route(
        "/v1/k/search",
        any(|| async { (StatusCode::IM_A_TEAPOT, "not a Ratatoskr envelope") }),
    ))
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let app = app(&harness, gateway(address, GatewayRouteClass::Control), 15);

    let (status, body) = send(&app, request(Some(CREDENTIAL), Body::empty())).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["code"], "edge.upstream_invalid_response");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// A listener that refuses connections is a truthful retryable service unavailability, never 200.
#[tokio::test]
async fn refused_downstream_connection_is_a_truthful_503() {
    let unavailable = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = unavailable.local_addr().expect("a listener address");
    drop(unavailable);
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let app = app(&harness, gateway(address, GatewayRouteClass::Control), 15);

    let (status, body) = send(&app, request(Some(CREDENTIAL), Body::empty())).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "edge.upstream_unavailable");
    harness.cleanup().await.expect("cleanup");
}

/// Edge removes caller-controlled credentials and reserved headers, then mints exactly the three
/// bounded claims the loopback service may trust.
#[tokio::test]
async fn edge_mints_bounded_identity_claims_and_strips_hop_headers() {
    let (headers_tx, mut headers_rx) = mpsc::channel::<HeaderMap>(1);
    let (address, task) = stub(Router::new().route(
        "/v1/k/search",
        any(move |request: axum::extract::Request| {
            let headers_tx = headers_tx.clone();
            async move {
                headers_tx
                    .send(request.headers().clone())
                    .await
                    .expect("record downstream headers");
                StatusCode::NO_CONTENT
            }
        }),
    ))
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let app = app(&harness, gateway(address, GatewayRouteClass::Control), 15);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/k/search")
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("cookie", "session=forged")
        .header("x-ratatoskr-user-id", "forged")
        .header("connection", "x-remove-me")
        .header("x-remove-me", "forged")
        .body(Body::empty())
        .expect("a request");

    let (status, _) = send(&app, request).await;
    let headers = headers_rx.recv().await.expect("one downstream call");

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("cookie").is_none());
    assert!(headers.get("x-remove-me").is_none());
    assert!(headers.get("x-ratatoskr-user-id").is_some());
    assert!(headers.get("x-correlation-id").is_some());
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// A class body limit is enforced after the broad public listener limit and before the downstream
/// sees a request, so a transfer-sized listener cannot accidentally make control routes unbounded.
#[tokio::test]
async fn route_class_body_budget_refuses_before_calling_downstream() {
    let (called_tx, mut called_rx) = mpsc::channel::<()>(1);
    let (address, task) = stub(Router::new().route(
        "/v1/k/search",
        any(move || {
            let called_tx = called_tx.clone();
            async move {
                called_tx.send(()).await.expect("record downstream call");
                StatusCode::OK
            }
        }),
    ))
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let gateway = with_control_budget(gateway(address, GatewayRouteClass::Control), 1024, 15);
    let app = app(&harness, gateway, 15);
    let body = vec![b'x'; 1025];
    let request = Request::builder()
        .method("POST")
        .uri("/v1/k/search")
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("content-length", body.len())
        .body(Body::from(body))
        .expect("a request");

    let (status, body) = send(&app, request).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["code"], "platform.request.payload_too_large");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), called_rx.recv())
            .await
            .is_err(),
        "an oversized request reached the downstream"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// The route class owns a downstream-header timeout, and tells a caller that the *service* was
/// slow rather than blaming the client with a 408 or pretending success.
#[tokio::test]
async fn slow_downstream_is_a_route_budget_504() {
    let (address, task) = stub(Router::new().route(
        "/v1/k/search",
        any(|| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            StatusCode::OK
        }),
    ))
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let gateway = with_control_budget(gateway(address, GatewayRouteClass::Control), 1024, 1);
    let app = app(&harness, gateway, 2);

    let (status, body) = send(&app, request(Some(CREDENTIAL), Body::empty())).await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["code"], "edge.upstream_timeout");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// Downstream hop-by-hop and reserved headers cannot cross the loopback trust boundary back to a
/// client. End-to-end headers survive unchanged.
#[tokio::test]
async fn downstream_hop_headers_are_not_exposed_to_clients() {
    let (address, task) = stub(Router::new().route(
        "/v1/k/search",
        any(|| async {
            let mut response = axum::response::Response::new(Body::from("ok"));
            response
                .headers_mut()
                .insert("connection", "x-upstream-only".parse().expect("a header"));
            response
                .headers_mut()
                .insert("x-upstream-only", "no".parse().expect("a header"));
            response
                .headers_mut()
                .insert("x-ratatoskr-user-id", "forged".parse().expect("a header"));
            response
                .headers_mut()
                .insert("x-domain-trace", "kept".parse().expect("a header"));
            response
        }),
    ))
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let app = app(&harness, gateway(address, GatewayRouteClass::Control), 15);

    let response = app
        .oneshot(request(Some(CREDENTIAL), Body::empty()))
        .await
        .expect("a response");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("connection").is_none());
    assert!(response.headers().get("x-upstream-only").is_none());
    assert!(response.headers().get("x-ratatoskr-user-id").is_none());
    assert_eq!(response.headers()["x-domain-trace"], "kept");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// A fixture sends two SSE events apart. Edge must return the first event before the second exists,
/// preserve their order, and not collect the body before giving the caller its response.
#[tokio::test]
async fn sse_pass_through_preserves_event_order_and_flush_timing() {
    let (address, task) = stub(Router::new().route(
        "/v1/k/events",
        any(|| async {
            let stream = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"event: one\ndata: 1\n\n"));
                tokio::time::sleep(Duration::from_millis(120)).await;
                yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"event: two\ndata: 2\n\n"));
            };
            (
                [("content-type", "text/event-stream"), ("cache-control", "no-cache")],
                Body::from_stream(stream),
            )
        }),
    ))
    .await;
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let app = app(&harness, gateway(address, GatewayRouteClass::Stream), 300);
    let request = Request::builder()
        .method("GET")
        .uri("/v1/k/events")
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .body(Body::empty())
        .expect("a request");
    let started = tokio::time::Instant::now();

    let response = app.oneshot(request).await.expect("a response");
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_millis(60), body.next())
        .await
        .expect("the first SSE event flushes promptly")
        .expect("a first chunk")
        .expect("a valid first chunk");
    assert!(started.elapsed() < Duration::from_millis(80));
    assert_eq!(first.as_ref(), b"event: one\ndata: 1\n\n");
    assert!(
        tokio::time::timeout(Duration::from_millis(40), body.next())
            .await
            .is_err(),
        "the proxy buffered until the delayed second event"
    );
    let second = tokio::time::timeout(Duration::from_millis(160), body.next())
        .await
        .expect("the delayed event arrives")
        .expect("a second chunk")
        .expect("a valid second chunk");
    assert_eq!(second.as_ref(), b"event: two\ndata: 2\n\n");
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// Capability aggregation retains the last service-owned document and marks its timestamp stale
/// when the next loopback refresh cannot connect; it never replaces failure with an empty success.
#[tokio::test]
async fn capability_aggregation_marks_a_failed_refresh_stale() {
    let (address, task) = stub(Router::new().route(
        "/v1/capabilities",
        any(|| async { axum::Json(serde_json::json!({"features": ["search"]})) }),
    ))
    .await;
    let gateway = platform_public_api::gateway::Gateway::from_config(&gateway(
        address,
        GatewayRouteClass::Control,
    ));

    gateway.refresh_capabilities().await;
    let fresh = gateway.capabilities().await;
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].document["features"], serde_json::json!(["search"]));
    assert!(!fresh[0].stale);
    assert!(fresh[0].observed_at.is_some());
    task.abort();
    task.await.expect_err("fixture task is aborted");

    gateway.refresh_capabilities().await;
    let stale = gateway.capabilities().await;
    assert!(stale[0].stale);
    assert!(stale[0].observed_at.is_some());
    assert!(stale[0].stale_since.is_some());
    assert_eq!(stale[0].document["features"], serde_json::json!(["search"]));
}

/// Two independent domain listeners are composed under one public Edge listener; a request cannot
/// cross from one configured prefix to the other's internal listener.
#[tokio::test]
async fn two_downstream_services_are_routed_by_their_public_prefixes() {
    let (knowledge_address, knowledge_task) = stub(Router::new().route(
        "/v1/k/search",
        any(|| async { (StatusCode::OK, "knowledge") }),
    ))
    .await;
    let (github_address, github_task) =
        stub(Router::new().route("/v1/gh/repos", any(|| async { (StatusCode::OK, "github") })))
            .await;
    let mut routes = gateway(knowledge_address, GatewayRouteClass::Control).routes;
    routes.insert(
        "github".to_owned(),
        GatewayRouteConfig {
            prefix: "/v1/gh".to_owned(),
            listener: github_address,
            class: Some(GatewayRouteClass::Control),
            capabilities_path: "/v1/capabilities".to_owned(),
            archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
        },
    );
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let app = app(
        &harness,
        GatewayConfig {
            routes,
            ..GatewayConfig::default()
        },
        15,
    );
    for path in ["/v1/k/search", "/v1/gh/repos"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {CREDENTIAL}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("a response");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    knowledge_task.abort();
    github_task.abort();
    harness.cleanup().await.expect("cleanup");
}
