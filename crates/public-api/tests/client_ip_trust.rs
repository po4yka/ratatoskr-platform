//! When Edge trusts `X-Ratatoskr-Client-IP` (workspace change `add-site-gateway-hosting`, spec
//! `site-gateway`, "The gateway sets the client address that Edge rate-limits on").
//!
//! The sites gateway sets the header and connects to Edge from its own loopback address,
//! `127.0.0.83` by default. Edge keys the per-client bucket of the `public` class on the header
//! only on a connection from that address, and on the peer address on every other connection. The
//! header never reaches a downstream service.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request as Received};
use axum::routing::any;
use http::{Request, StatusCode};
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::ApiState;
use platform_public_api::sites::PublicLimits;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt as _;

const PAGE: &str = "/v1/sites/site-a/content/pages/home";

/// The default source address of the sites gateway (design D7 of the workspace change).
const GATEWAY_PEER: &str = "127.0.0.83:40000";

/// What the stub sites service received: the value of `x-ratatoskr-client-ip` of each request.
type Seen = Arc<Mutex<Vec<Option<String>>>>;

async fn stub_sites() -> (SocketAddr, Seen) {
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    let router = Router::new().route(
        "/v1/sites/{*rest}",
        any(move |request: Received| {
            let record = Arc::clone(&record);
            async move {
                let header = request
                    .headers()
                    .get("x-ratatoskr-client-ip")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                record.lock().expect("the record").push(header);
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
    (address, seen)
}

/// Edge's public router with the sites route, and a client bucket of one request a minute, so
/// that the second request of one key is refused.
fn app(harness: &TestDatabase, sites: SocketAddr) -> Router {
    let gateway = GatewayConfig {
        routes: BTreeMap::from([(
            "sites".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/sites".to_owned(),
                listener: sites,
                class: Some(GatewayRouteClass::Control),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        )]),
        ..GatewayConfig::default()
    };
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), "edge", health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(&gateway);
    state.site_limits = Arc::new(PublicLimits::new(1, 1000));
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

/// One request over a real loopback connection from `127.0.0.1`. Answers the status.
async fn over_loopback(edge: SocketAddr, client_ip: &str) -> u16 {
    let mut stream = TcpStream::connect(edge).await.expect("a connection");
    let request = format!(
        "GET {PAGE} HTTP/1.1\r\nHost: edge\r\nX-Ratatoskr-Client-IP: {client_ip}\r\n\
         Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request is sent");
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .await
        .expect("the answer is read");
    answer
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("an HTTP status line: {answer}"))
}

/// One request that the router receives as if from `peer`.
async fn from_peer(app: &Router, peer: &str, client_ip: &str) -> u16 {
    let mut request = Request::builder()
        .uri(PAGE)
        .header("x-ratatoskr-client-ip", client_ip)
        .body(Body::empty())
        .expect("a request");
    let peer: SocketAddr = peer.parse().expect("a peer address");
    request.extensions_mut().insert(ConnectInfo(peer));
    app.clone()
        .oneshot(request)
        .await
        .expect("a response")
        .status()
        .as_u16()
}

/// Spec `site-gateway`, "Another local peer sends the header to Edge": from `127.0.0.1`, which is
/// not the gateway address, Edge keys the client bucket on the peer and not on the header, and the
/// downstream request carries no `X-Ratatoskr-Client-IP`.
#[tokio::test]
async fn client_ip_header_is_ignored_from_other_peers() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (sites, seen) = stub_sites().await;
    let app = app(&harness, sites);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let edge = listener.local_addr().expect("a listener address");
    let served = platform_http::serve(listener, app.clone());

    assert_eq!(
        over_loopback(edge, "3.3.3.3").await,
        200,
        "the first request"
    );
    assert_eq!(
        over_loopback(edge, "5.5.5.5").await,
        429,
        "a second header value from the same peer spends the same bucket"
    );
    assert_eq!(
        from_peer(&app, GATEWAY_PEER, "127.0.0.1").await,
        429,
        "the spent bucket is the bucket of the key 127.0.0.1, the peer address"
    );
    assert_eq!(
        seen.lock().expect("the record").as_slice(),
        [None],
        "one request reached the sites service, without the header"
    );

    drop(served);
    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-gateway`, "The gateway's header is trusted": from the gateway address, Edge keys the
/// client bucket on the value of the header.
#[tokio::test]
async fn client_ip_header_is_used_from_the_gateway_peer() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (sites, seen) = stub_sites().await;
    let app = app(&harness, sites);

    assert_eq!(from_peer(&app, GATEWAY_PEER, "4.4.4.4").await, 200);
    assert_eq!(
        from_peer(&app, GATEWAY_PEER, "6.6.6.6").await,
        200,
        "another client behind the gateway has its own bucket"
    );
    assert_eq!(
        from_peer(&app, GATEWAY_PEER, "4.4.4.4").await,
        429,
        "the key is 4.4.4.4, and its bucket is spent"
    );
    assert_eq!(
        seen.lock().expect("the record").as_slice(),
        [None, None],
        "the header never reaches the sites service"
    );

    harness.cleanup().await.expect("cleanup");
}
