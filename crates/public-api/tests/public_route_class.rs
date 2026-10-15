//! The `public` route class of ADR-0017 (workspace change `add-site-publications`, spec
//! `site-content-delivery`): the Content Delivery API of `ratatoskr-sites` through Edge, against a
//! stub sites service that counts the requests it receives.

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
use axum::extract::Request as Received;
use axum::routing::any;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const AUDIENCE: &str = "edge";
const OWNER_CREDENTIAL: &str = "public-class-owner-credential-0000";
const PAGE: &str = "/v1/sites/demo-site/content/pages/home";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// A primary browser session whose user holds the live owner grant.
async fn seed_owner(pool: &sqlx::PgPool) {
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
    platform_identity::grant::grant(pool, user.user_id, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("the owner grant");
}

/// Approve `site` in the database and serve its active manifest from the stub, both with the
/// public capabilities, so that Edge's capability check of the content routes passes (workspace
/// spec `site-capability-enforcement`).
async fn approve(pool: &sqlx::PgPool, site: &str) {
    let user = platform_identity::user::create_user(pool, jiff::Timestamp::now())
        .await
        .expect("a user");
    sqlx::query(
        "insert into identity.site_approvals
             (site_id, changed_by, manifest_sha256, capabilities, hostname, state, changed_at)
         values ($1, $2, $3, $4, 'garden.example.org', 'approved', now())",
    )
    .bind(site)
    .bind(user.user_id)
    .bind("a".repeat(64))
    .bind(vec![
        "pages.read",
        "assets.read",
        "search.publication",
        "live.release",
    ])
    .execute(pool)
    .await
    .expect("an approval");
}

/// The active manifest that the stub sites service answers for every site.
async fn active_manifest(
    axum::extract::Path(site): axum::extract::Path<String>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "site_id": site,
        "manifest_sha256": "a".repeat(64),
        "hostname": "garden.example.org",
        "publications": ["garden"],
        "capabilities": ["pages.read", "assets.read", "search.publication", "live.release"],
    }))
}

/// A stub sites service. It counts every request, and its answer repeats what it received of the
/// caller's identity, so an answer that a credential changed would differ.
async fn stub_sites() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let router = Router::new()
        .route(
            "/internal/sites/{site}/manifest",
            axum::routing::get(active_manifest),
        )
        .route(
            "/v1/sites/{*rest}",
            any(move |request: Received| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let header = |name: &str| {
                        request
                            .headers()
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned)
                    };
                    let body = serde_json::json!({
                        "path": request.uri().path(),
                        "authorization": header("authorization"),
                        "cookie": header("cookie"),
                        "user": header("x-ratatoskr-user-id"),
                        "device": header("x-ratatoskr-device-id"),
                        "session_kind": header("x-ratatoskr-session-kind"),
                    });
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .header("cache-control", "public, max-age=60")
                        .header("etag", "\"release-1\"")
                        .header("set-cookie", "visitor=1; Path=/")
                        .body(Body::from(body.to_string()))
                        .expect("a stub answer")
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

/// Edge's public router with the sites route of the deployment example.
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
        max_concurrent_requests: 1024,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(Arc::new(state)),
    )
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a response body")
        .to_bytes();
    (status, headers, bytes.to_vec())
}

fn code(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("code")
                .and_then(|code| code.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// The headers of an answer that do not change from one request to the next.
fn stable(headers: &HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut stable = BTreeMap::new();
    for (name, value) in headers {
        if matches!(name.as_str(), "date" | "x-correlation-id") {
            continue;
        }
        stable
            .entry(name.as_str().to_owned())
            .or_insert_with(Vec::new)
            .push(value.to_str().unwrap_or("<binary>").to_owned());
    }
    stable
}

/// Spec `site-content-delivery`, "Other methods are refused at Edge": a write method on a content
/// route is 405 with the contract envelope, and the sites service receives nothing.
#[tokio::test]
async fn write_method_is_refused_before_proxy() {
    let harness = TestDatabase::create().await.expect("a test database");
    approve(harness.pool(), "demo-site").await;
    let (listener, count) = stub_sites().await;
    let app = app(&harness, listener);

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let (status, _, body) = send(
            &app,
            Request::builder()
                .method(method)
                .uri(PAGE)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("a request"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {PAGE}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(code(&body), "platform.route.method_not_allowed");
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "a refused method reached the sites service"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-content-delivery`, "The public route class is anonymous and identical for every
/// caller": no credential, an invalid one and a valid owner session get the same answer, and no
/// answer sets a cookie.
#[tokio::test]
async fn credential_does_not_change_a_public_response() {
    let harness = TestDatabase::create().await.expect("a test database");
    approve(harness.pool(), "demo-site").await;
    seed_owner(harness.pool()).await;
    let (listener, count) = stub_sites().await;
    let app = app(&harness, listener);

    let anonymous = send(
        &app,
        Request::builder()
            .uri(PAGE)
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(
        anonymous.0,
        StatusCode::OK,
        "an anonymous page read: {}",
        String::from_utf8_lossy(&anonymous.2)
    );
    for (label, credential) in [
        ("an invalid credential", "not-a-session-credential"),
        ("the owner session", OWNER_CREDENTIAL),
    ] {
        let answer = send(
            &app,
            Request::builder()
                .uri(PAGE)
                .header("authorization", format!("Bearer {credential}"))
                .header("cookie", "ratatoskr_session=owner")
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(answer.0, anonymous.0, "status with {label}");
        assert_eq!(
            stable(&answer.1),
            stable(&anonymous.1),
            "headers with {label}"
        );
        assert_eq!(
            String::from_utf8_lossy(&answer.2),
            String::from_utf8_lossy(&anonymous.2),
            "body with {label}"
        );
        assert!(
            answer.1.get("set-cookie").is_none(),
            "an answer with {label} sets a cookie"
        );
    }
    assert!(
        anonymous.1.get("set-cookie").is_none(),
        "the answer sets a cookie"
    );
    assert_eq!(
        anonymous
            .1
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=60"),
        "the sites service owns Cache-Control"
    );
    assert_eq!(
        anonymous.1.get("etag").and_then(|v| v.to_str().ok()),
        Some("\"release-1\""),
        "the sites service owns ETag"
    );
    let seen: serde_json::Value = serde_json::from_slice(&anonymous.2).expect("the stub answer");
    assert_eq!(
        seen["session_kind"], "anonymous",
        "the anonymous claim: {seen}"
    );
    assert_eq!(seen["user"], serde_json::Value::Null, "{seen}");
    assert_eq!(count.load(Ordering::SeqCst), 3);

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-content-delivery`, "Public requests are rate-limited before any work": one client
/// over its bucket gets 429 with `Retry-After`, the stub counts only the admitted requests, and
/// another client is still admitted.
#[tokio::test]
async fn per_client_limit_is_enforced_before_proxy() {
    let harness = TestDatabase::create().await.expect("a test database");
    approve(harness.pool(), "demo-site").await;
    let (listener, count) = stub_sites().await;
    let app = app(&harness, listener);
    // From the sites gateway's address, the only peer whose X-Ratatoskr-Client-IP Edge reads
    // (workspace spec `site-gateway`).
    let gateway: std::net::SocketAddr = "127.0.0.83:40000".parse().expect("a peer address");
    let read = |client: &str| {
        let mut request = Request::builder()
            .uri(PAGE)
            .header("x-ratatoskr-client-ip", client)
            .body(Body::empty())
            .expect("a request");
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(gateway));
        request
    };

    let mut admitted = 0_usize;
    let mut refused = None;
    for _ in 0..400 {
        let (status, headers, body) = send(&app, read("203.0.113.7")).await;
        if status == StatusCode::OK {
            admitted += 1;
            continue;
        }
        refused = Some((status, headers, body));
        break;
    }
    let Some((status, headers, body)) = refused else {
        panic!("400 requests of one client were all admitted ({admitted} reached the stub)");
    };
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the excess requests are answered 429: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(code(&body), "platform.limit.rate_exceeded");
    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    assert!(
        retry_after.is_some_and(|seconds| seconds > 0),
        "a refusal carries Retry-After: {headers:?}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        admitted,
        "the stub counts only the admitted requests"
    );

    let (status, _, _) = send(&app, read("198.51.100.9")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "another client keeps its own bucket"
    );
    assert_eq!(count.load(Ordering::SeqCst), admitted + 1);

    harness.cleanup().await.expect("cleanup");
}

/// Review L2 of SITE-027: a content path of one site whose encoded dot segments resolve to another
/// site under `/v1/sites` is refused before the sites service.
#[tokio::test]
async fn content_path_resolving_to_another_site_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    approve(harness.pool(), "demo-site").await;
    let (listener, count) = stub_sites().await;
    let app = app(&harness, listener);

    for path in [
        "/v1/sites/demo-site/content/pages/..%2f..%2f..%2fother-site%2fcontent%2fpages%2fhome",
        "/v1/sites/demo-site/content/pages/%2e%2e/%2e%2e/%2e%2e/other-site/content/pages/home",
    ] {
        let (status, _, body) = send(
            &app,
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::NOT_FOUND, "platform.route.not_found".to_owned()),
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "a path of another site reached the sites service"
    );

    harness.cleanup().await.expect("cleanup");
}
