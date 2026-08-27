//! Anonymous sanitized public status through the real Edge router.

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
use axum::routing::get;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::ApiState;
use ratatoskr_operational_contracts::{
    PublicComponentId, PublicComponentState, PublicStatusDocument, PublicStatusState,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

const SERVICE_NAME: &str = "private-knowledge";
const ROUTE_PREFIX: &str = "/v1/private-knowledge";
const CAPABILITIES_PATH: &str = "/internal/private-capabilities";
const PRIVATE_URL: &str = "https://private.internal/archive?token=status-secret";
const PRIVATE_TOKEN: &str = "status-private-token";
const PRIVATE_DIAGNOSTIC: &str = "connection refused at storage.internal:5432";

fn gateway_config(listener: std::net::SocketAddr) -> GatewayConfig {
    GatewayConfig {
        routes: BTreeMap::from([(
            SERVICE_NAME.to_owned(),
            GatewayRouteConfig {
                prefix: ROUTE_PREFIX.to_owned(),
                listener,
                class: Some(GatewayRouteClass::Control),
                capabilities_path: CAPABILITIES_PATH.to_owned(),
                archive_receipt_path: "/internal/archive-receipt".to_owned(),
            },
        )]),
        ..GatewayConfig::default()
    }
}

fn build_app(
    harness: &TestDatabase,
    health: Arc<RuntimeState>,
    gateway: platform_public_api::gateway::Gateway,
) -> Router {
    let mut state = ApiState::new(harness.database.clone(), "edge", health, true);
    state.gateway = gateway;
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

async fn stub(called: mpsc::Sender<()>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("a listener address");
    let router = Router::new().route(
        CAPABILITIES_PATH,
        get(move || {
            let called = called.clone();
            async move {
                called.send(()).await.expect("record a refresh");
                axum::Json(serde_json::json!({
                    "features": ["search", "analysis"],
                    "service": SERVICE_NAME,
                    "private_url": PRIVATE_URL,
                    "token": PRIVATE_TOKEN,
                    "diagnostic": PRIVATE_DIAGNOSTIC,
                }))
            }
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the fixture server serves");
    });
    (address, task)
}

fn request(authorization: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri("/v1/status");
    if let Some(authorization) = authorization {
        builder = builder.header("authorization", authorization);
    }
    builder.body(Body::empty()).expect("a request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, body)
}

fn document(body: &serde_json::Value) -> PublicStatusDocument {
    serde_json::from_value(body.clone()).expect("the shared public status contract")
}

fn assert_sanitized(body: &serde_json::Value, listener: std::net::SocketAddr) {
    let wire = serde_json::to_string(body).expect("JSON");
    for private in [
        SERVICE_NAME,
        ROUTE_PREFIX,
        CAPABILITIES_PATH,
        PRIVATE_URL,
        PRIVATE_TOKEN,
        PRIVATE_DIAGNOSTIC,
        "features",
        "analysis",
    ] {
        assert!(!wire.contains(private), "leaked {private}: {body}");
    }
    assert!(
        !wire.contains(&listener.to_string()),
        "leaked loopback listener: {body}"
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one status scenario preserves the ordered healthy, stale, unavailable, and unknown evidence"
)]
async fn public_status_is_anonymous_degraded_and_sanitized() {
    let (called_tx, mut called_rx) = mpsc::channel(4);
    let (listener, task) = stub(called_tx).await;
    let gateway = platform_public_api::gateway::Gateway::from_config(&gateway_config(listener));
    gateway.refresh_capabilities().await;
    called_rx.recv().await.expect("one explicit cached refresh");

    let harness = TestDatabase::create().await.expect("a test database");
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    health.set_bus_reachable(true);
    let app = build_app(&harness, Arc::clone(&health), gateway.clone());

    let (status, headers, anonymous_body) = send(&app, request(None)).await;
    assert_eq!(status, StatusCode::OK, "{anonymous_body}");
    assert_eq!(headers[http::header::CACHE_CONTROL], "no-store");
    let anonymous = document(&anonymous_body);
    assert_eq!(anonymous.state, PublicStatusState::Operational);
    assert_eq!(
        anonymous
            .components
            .iter()
            .map(|component| component.id)
            .collect::<Vec<_>>(),
        PublicStatusDocument::COMPONENT_ORDER
    );
    assert!(anonymous.components.iter().all(|component| {
        component.state == PublicComponentState::Operational && !component.stale
    }));
    let connected_observed_at = anonymous.components[3]
        .observed_at
        .expect("a successful connected-service observation");
    assert_sanitized(&anonymous_body, listener);

    let (garbage_status, garbage_headers, garbage_body) =
        send(&app, request(Some("Bearer definitely-not-a-session"))).await;
    assert_eq!(garbage_status, StatusCode::OK, "{garbage_body}");
    assert_eq!(garbage_headers[http::header::CACHE_CONTROL], "no-store");
    let garbage = document(&garbage_body);
    assert_eq!(garbage.state, anonymous.state);
    assert_eq!(garbage.components, anonymous.components);
    assert_sanitized(&garbage_body, listener);

    assert!(
        tokio::time::timeout(Duration::from_millis(50), called_rx.recv())
            .await
            .is_err(),
        "GET /v1/status performed request-time downstream I/O"
    );

    task.abort();
    task.await.expect_err("the fixture task is aborted");
    gateway.refresh_capabilities().await;

    let (status, headers, stale_body) = send(&app, request(None)).await;
    assert_eq!(status, StatusCode::OK, "{stale_body}");
    assert_eq!(headers[http::header::CACHE_CONTROL], "no-store");
    let stale = document(&stale_body);
    assert_eq!(stale.state, PublicStatusState::Degraded);
    assert_eq!(stale.components[3].id, PublicComponentId::ConnectedServices);
    assert_eq!(stale.components[3].state, PublicComponentState::Degraded);
    assert!(stale.components[3].stale);
    assert_eq!(
        stale.components[3].observed_at.as_ref(),
        Some(&connected_observed_at)
    );
    assert_sanitized(&stale_body, listener);

    health.set_database_reachable(false);
    health.set_bus_reachable(false);
    let (status, _, unavailable_body) = send(&app, request(None)).await;
    assert_eq!(status, StatusCode::OK, "{unavailable_body}");
    let unavailable = document(&unavailable_body);
    assert_eq!(unavailable.state, PublicStatusState::Unavailable);
    assert_eq!(unavailable.components[0].id, PublicComponentId::Api);
    assert_eq!(
        unavailable.components[0].state,
        PublicComponentState::Operational
    );
    assert_eq!(unavailable.components[1].id, PublicComponentId::Storage);
    assert_eq!(
        unavailable.components[1].state,
        PublicComponentState::Unavailable
    );
    assert_eq!(
        unavailable.components[2].id,
        PublicComponentId::CommandDelivery
    );
    assert_eq!(
        unavailable.components[2].state,
        PublicComponentState::Unavailable
    );
    assert_eq!(
        unavailable.components[3].state,
        PublicComponentState::Degraded
    );
    assert_sanitized(&unavailable_body, listener);

    let never_observed_health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    let never_observed_gateway =
        platform_public_api::gateway::Gateway::from_config(&gateway_config(listener));
    let never_observed_app = build_app(&harness, never_observed_health, never_observed_gateway);
    let (status, _, unknown_body) = send(&never_observed_app, request(None)).await;
    assert_eq!(status, StatusCode::OK, "{unknown_body}");
    let unknown = document(&unknown_body);
    assert_eq!(unknown.state, PublicStatusState::Degraded);
    assert_eq!(unknown.components[1].state, PublicComponentState::Unknown);
    assert_eq!(unknown.components[2].state, PublicComponentState::Unknown);
    assert_eq!(unknown.components[3].state, PublicComponentState::Unknown);
    assert!(unknown.components[1].observed_at.is_none());
    assert!(unknown.components[2].observed_at.is_none());
    assert!(unknown.components[3].observed_at.is_none());
    assert_sanitized(&unknown_body, listener);

    harness.cleanup().await.expect("cleanup");
}
