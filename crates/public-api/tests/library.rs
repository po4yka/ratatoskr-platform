//! Public library search and read-state acceptance tests.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
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
use uuid::Uuid;

const AUDIENCE: &str = "edge";
const CREDENTIAL: &str = "library-credential-000000000000";

#[derive(Debug)]
struct RecordedSearch {
    query: BTreeMap<String, String>,
    headers: HeaderMap,
}

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
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

fn gateway(listener: std::net::SocketAddr) -> GatewayConfig {
    GatewayConfig {
        routes: BTreeMap::from([(
            "knowledge".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/k".to_owned(),
                listener,
                class: Some(GatewayRouteClass::Control),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        )]),
        ..GatewayConfig::default()
    }
}

fn app(harness: &TestDatabase, gateway: &GatewayConfig) -> Router {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(gateway);
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

fn search_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("x-ratatoskr-user-id", Uuid::nil().to_string())
        .body(Body::empty())
        .expect("a request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a response body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, body)
}

/// The public route derives tenant authority from its principal and minimizes Knowledge data.
#[tokio::test]
async fn search_uses_principal_tenant_and_returns_only_public_fields() {
    let harness = TestDatabase::create().await.expect("a test database");
    let user_id = seed(harness.pool()).await;
    let (calls_tx, mut calls_rx) = mpsc::channel(1);
    let (address, task) = stub(
        Router::new()
            .route(
                "/internal/search",
                get(
                    |State(calls): State<mpsc::Sender<RecordedSearch>>,
                     Query(query): Query<BTreeMap<String, String>>,
                     headers: HeaderMap| async move {
                        calls
                            .send(RecordedSearch { query, headers })
                            .await
                            .expect("the test records the call");
                        axum::Json(serde_json::json!({
                            "results": [{
                                "analysis_id": "019d3d22-7631-74b0-beb2-98bf7619e5c1",
                                "document_id": "019d3d22-7631-74b0-beb2-98bf7619e5c2",
                                "owner_context": "must-not-cross-edge",
                                "title": "Bounded title",
                                "snippet": "Useful result",
                                "rank": 0.75,
                                "read_state": "unread",
                                "tenant_ref": "must-not-cross-edge"
                            }],
                            "has_more": true
                        }))
                    },
                ),
            )
            .with_state(calls_tx),
    )
    .await;
    let app = app(&harness, &gateway(address));

    let (status, headers, body) = send(
        &app,
        search_request("/v1/library/search?q=bounded&read_state=unread&limit=5&offset=2"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    assert_eq!(
        body,
        serde_json::json!({
            "items": [{
                "analysis_id": "019d3d22-7631-74b0-beb2-98bf7619e5c1",
                "document_id": "019d3d22-7631-74b0-beb2-98bf7619e5c2",
                "title": "Bounded title",
                "snippet": "Useful result",
                "score": 0.75,
                "read_state": "unread"
            }],
            "limit": 5,
            "offset": 2,
            "has_more": true
        })
    );
    let recorded = calls_rx.recv().await.expect("one Knowledge call");
    assert_eq!(
        recorded.query.get("tenant"),
        Some(&format!("user:{user_id}"))
    );
    assert_eq!(recorded.query.get("q").map(String::as_str), Some("bounded"));
    assert_eq!(
        recorded.query.get("read_state").map(String::as_str),
        Some("unread")
    );
    assert_eq!(recorded.query.get("limit").map(String::as_str), Some("5"));
    assert_eq!(recorded.query.get("offset").map(String::as_str), Some("2"));
    assert!(recorded.headers.get("authorization").is_none());
    assert!(recorded.headers.get("x-ratatoskr-user-id").is_none());
    assert!(calls_rx.try_recv().is_err());
    task.abort();
}

/// Invalid public input, including identity selectors, never reaches the loopback owner.
#[tokio::test]
async fn invalid_search_input_and_forged_identity_stop_before_knowledge() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&calls);
    let (address, task) = stub(Router::new().route(
        "/internal/search",
        get(move || {
            recorded.fetch_add(1, Ordering::SeqCst);
            async {
                axum::Json(serde_json::json!({
                    "results": [],
                    "has_more": false
                }))
            }
        }),
    ))
    .await;
    let app = app(&harness, &gateway(address));
    let oversized = "x".repeat(513);
    let cases = [
        format!("/v1/library/search?q={oversized}"),
        "/v1/library/search?limit=0".to_owned(),
        "/v1/library/search?limit=101".to_owned(),
        "/v1/library/search?offset=-1".to_owned(),
        "/v1/library/search?offset=18446744073709551615".to_owned(),
        "/v1/library/search?read_state=maybe".to_owned(),
        "/v1/library/search?unexpected=true".to_owned(),
        "/v1/library/search?tenant=user%3Aforeign".to_owned(),
    ];

    for uri in cases {
        let (status, _, body) = send(&app, search_request(&uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(body["code"], "platform.request.invalid", "{uri}: {body}");
    }

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}

fn read_state_request(analysis_id: Uuid, body: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/v1/library/items/{analysis_id}/read-state"))
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

/// Replacement is naturally idempotent and hides scoped absence behind one public error.
#[tokio::test]
async fn read_state_put_is_idempotent_and_hides_foreign_targets() {
    const OWNER_ITEM: Uuid = Uuid::from_u128(0x019d_3d22_7631_74b0_beb2_98bf_7619_e5c1);
    const FOREIGN_ITEM: Uuid = Uuid::from_u128(0x019d_3d22_7631_74b0_beb2_98bf_7619_e5c2);
    const MISSING_ITEM: Uuid = Uuid::from_u128(0x019d_3d22_7631_74b0_beb2_98bf_7619_e5c3);

    let harness = TestDatabase::create().await.expect("a test database");
    let user_id = seed(harness.pool()).await;
    let expected_tenant = format!("user:{user_id}");
    let calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    let (address, task) = stub(Router::new().route(
        "/internal/user-content/command",
        post(move |Json(body): Json<serde_json::Value>| {
            let recorded = Arc::clone(&recorded);
            let expected_tenant = expected_tenant.clone();
            async move {
                recorded.lock().await.push(body.clone());
                assert_eq!(body["operation"], "set_read_state");
                assert_eq!(body["tenant"], expected_tenant);
                assert_eq!(body["read_state"], "read");
                let output = body["output_id"].as_str().expect("an output identity");
                if output == OWNER_ITEM.to_string() {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({"read_state":"read"})),
                    )
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({"error":"user_content_not_found"})),
                    )
                }
            }
        }),
    ))
    .await;
    let app = app(&harness, &gateway(address));

    for _ in 0..2 {
        let (status, _, body) = send(
            &app,
            read_state_request(OWNER_ITEM, r#"{"read_state":"read"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, serde_json::json!({"read_state":"read"}));
    }

    let mut absences = Vec::new();
    for item in [FOREIGN_ITEM, MISSING_ITEM] {
        let (status, _, body) =
            send(&app, read_state_request(item, r#"{"read_state":"read"}"#)).await;
        absences.push((status, body["code"].clone(), body["retryable"].clone()));
    }
    assert_eq!(absences[0], absences[1]);
    assert_eq!(absences[0].0, StatusCode::NOT_FOUND);
    assert_eq!(absences[0].1, "platform.resource.not_found");

    let calls = calls.lock().await;
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|call| call.get("favorite").is_none()));
    task.abort();
}

/// Dependency timeouts and uncontracted successes become stable safe Platform failures.
#[tokio::test]
async fn knowledge_timeout_and_invalid_success_map_to_safe_errors() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool()).await;

    let (timeout_address, timeout_task) = stub(Router::new().route(
        "/internal/search",
        get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            Json(serde_json::json!({"results": [], "has_more": false}))
        }),
    ))
    .await;
    let mut timeout_gateway = gateway(timeout_address);
    timeout_gateway.budgets.control.response_timeout_seconds = 1;
    let timeout_app = app(&harness, &timeout_gateway);
    let (status, _, body) = send(&timeout_app, search_request("/v1/library/search?q=slow")).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "edge.upstream_timeout");
    timeout_task.abort();

    let marker = "private-upstream-topology";
    let oversized = serde_json::json!({
        "results": [],
        "has_more": false,
        "internal": format!("{marker}{}", "x".repeat(2048))
    });
    let (oversized_address, oversized_task) = stub(Router::new().route(
        "/internal/search",
        get(move || {
            let oversized = oversized.clone();
            async move { Json(oversized) }
        }),
    ))
    .await;
    let mut oversized_gateway = gateway(oversized_address);
    oversized_gateway.budgets.control.max_body_bytes = 1024;
    let oversized_app = app(&harness, &oversized_gateway);
    let (status, _, body) = send(
        &oversized_app,
        search_request("/v1/library/search?q=oversized"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "edge.upstream_invalid_response");
    assert!(!body.to_string().contains(marker));
    oversized_task.abort();

    let (malformed_address, malformed_task) = stub(Router::new().route(
        "/internal/search",
        get(move || async move {
            Json(serde_json::json!({
                "results": [{
                    "analysis_id": "019d3d22-7631-74b0-beb2-98bf7619e5c1",
                    "document_id": "019d3d22-7631-74b0-beb2-98bf7619e5c2",
                    "title": "unsafe",
                    "snippet": null,
                    "rank": 1.0,
                    "read_state": "secret-state",
                    "private_error": marker
                }],
                "has_more": false
            }))
        }),
    ))
    .await;
    let malformed_app = app(&harness, &gateway(malformed_address));
    let (status, _, body) = send(
        &malformed_app,
        search_request("/v1/library/search?q=malformed"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "edge.upstream_invalid_response");
    assert!(!body.to_string().contains(marker));
    malformed_task.abort();
}
