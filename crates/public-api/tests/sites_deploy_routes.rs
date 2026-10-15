//! The `deploy` routes under `/v1/sites/{site}/` (workspace change `add-site-gateway-hosting`,
//! spec `site-route-classes`, "A deploy route accepts the owner or the site's deployer device"),
//! against stub sites and notes services that record what they receive.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::Request as Received;
use axum::routing::any;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::pairing::{NewPairingCode, PairRequest};
use platform_identity::{DeviceKind, NewSession, SecretDigest, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const AUDIENCE: &str = "edge";
const MEMBER: &str = "deploy-routes-member-000000000";
const DEPLOYER: &str = "deploy-routes-deployer-0000000";
const SITE: &str = "site-a";
const OTHER_SITE: &str = "site-b";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const BUNDLE: &[u8] = b"a tar archive of the bundle";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// What one stub received: the path, the headers and the body.
type Seen = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;

/// A stub service under `prefix` that records every request and answers `200` with `{}`.
async fn stub(prefix: &str) -> (std::net::SocketAddr, Seen) {
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    let handler = any(move |request: Received| {
        let record = Arc::clone(&record);
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body.collect().await.expect("a body").to_bytes().to_vec();
            record.lock().expect("the record").push((
                parts.uri.path().to_owned(),
                parts.headers,
                bytes,
            ));
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("a stub answer")
        }
    });
    let router = Router::new()
        .route(prefix, handler.clone())
        .route(&format!("{prefix}/{{*rest}}"), handler);
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

fn route(prefix: &str, listener: std::net::SocketAddr) -> GatewayRouteConfig {
    GatewayRouteConfig {
        prefix: prefix.to_owned(),
        listener,
        class: Some(GatewayRouteClass::Control),
        capabilities_path: "/v1/capabilities".to_owned(),
        archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
    }
}

/// Edge's public router with the sites and notes routes of the deployment example.
fn app(harness: &TestDatabase, sites: std::net::SocketAddr, notes: std::net::SocketAddr) -> Router {
    let gateway = GatewayConfig {
        routes: BTreeMap::from([
            ("sites".to_owned(), route("/v1/sites", sites)),
            ("notes".to_owned(), route("/v1/notes", notes)),
        ]),
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

/// A user with a primary browser session whose credential is `credential`.
async fn browser_session(pool: &sqlx::PgPool, credential: &str) -> (uuid::Uuid, uuid::Uuid) {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let session = platform_identity::session::create_session(
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
    (user.user_id, session.session_id)
}

/// A `site_deployer` device paired for [`SITE`] through a pinned pairing code, whose first session
/// credential is [`DEPLOYER`]. Answers the device id.
async fn deployer(pool: &sqlx::PgPool) -> uuid::Uuid {
    let (owner, session) = browser_session(pool, "deploy-routes-owner-00000000000").await;
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &NewPairingCode {
            user_id: owner,
            created_by_session_id: session,
            expected_kind: Some(DeviceKind::SiteDeployer),
            site_id: Some(SITE),
            label: None,
            code_digest: SecretDigest::of("deploy-routes-pairing-code"),
            now: now(),
            expires_at: now() + jiff::SignedDuration::from_mins(10),
        },
    )
    .await
    .expect("a site_deployer code");
    let redeemed = platform_identity::pairing::redeem(
        &mut transaction,
        &PairRequest {
            presented: SecretDigest::of("deploy-routes-pairing-code"),
            declared_kind: DeviceKind::SiteDeployer,
            display_name: None,
            device_secret: SecretDigest::of("deploy-routes-device-secret"),
            access_token: auth::credential_digest(DEPLOYER),
            refresh_token: SecretDigest::of("deploy-routes-refresh-token"),
            audience: AUDIENCE,
            now: now(),
            access_expires_at: now() + jiff::SignedDuration::from_hours(1),
            refresh_expires_at: now() + jiff::SignedDuration::from_hours(24),
        },
    )
    .await
    .expect("redeem runs")
    .expect("the device pairs");
    transaction.commit().await.expect("commit");
    redeemed.device.device_id
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    credential: &str,
    body: &'static [u8],
) -> (u16, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/x-tar")
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from(body))
        .expect("a request");
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let code = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|body| {
            body.get("code")
                .and_then(|code| code.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    (status, code)
}

fn deploys(site: &str) -> String {
    format!("/v1/sites/{site}/deploys")
}

fn activate(site: &str) -> String {
    format!("/v1/sites/{site}/deploys/{COMMIT}/activate")
}

fn count(seen: &Seen) -> usize {
    seen.lock().expect("the record").len()
}

/// Spec `site-route-classes`, "A session without the owner grant deploys": a primary session
/// without `platform.owner` gets the stable forbidden error on both deploy routes, and the stub
/// sites service receives no request.
#[tokio::test]
async fn a_deploy_without_the_owner_grant_never_reaches_the_sites_service() {
    let harness = TestDatabase::create().await.expect("a test database");
    browser_session(harness.pool(), MEMBER).await;
    let (sites, seen) = stub("/v1/sites").await;
    let (notes, _) = stub("/v1/notes").await;
    let app = app(&harness, sites, notes);

    assert_eq!(
        call(&app, "POST", &deploys(SITE), MEMBER, BUNDLE).await,
        (403, "platform.auth.forbidden".to_owned()),
        "POST {}",
        deploys(SITE)
    );
    assert_eq!(
        call(&app, "POST", &activate(SITE), MEMBER, b"").await,
        (403, "platform.auth.forbidden".to_owned()),
        "POST {}",
        activate(SITE)
    );
    assert_eq!(
        count(&seen),
        0,
        "a refused deploy reached the sites service"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-route-classes`, "A deployer device of another site": a `site_deployer` paired for
/// [`SITE`] gets `edge.capability_not_granted` on the deploy routes of [`OTHER_SITE`], and the stub
/// sites service receives no request.
#[tokio::test]
async fn a_deployer_device_cannot_deploy_another_site() {
    let harness = TestDatabase::create().await.expect("a test database");
    deployer(harness.pool()).await;
    let (sites, seen) = stub("/v1/sites").await;
    let (notes, _) = stub("/v1/notes").await;
    let app = app(&harness, sites, notes);

    for (path, body) in [
        (deploys(OTHER_SITE), BUNDLE),
        (activate(OTHER_SITE), &b""[..]),
    ] {
        assert_eq!(
            call(&app, "POST", &path, DEPLOYER, body).await,
            (403, "edge.capability_not_granted".to_owned()),
            "POST {path}"
        );
    }
    assert_eq!(
        count(&seen),
        0,
        "a deploy to another site reached the sites service"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-route-classes`, "A deployer device on a management route": a `site_deployer` is
/// refused on a management route of its own site, on a notes route and on a route that other
/// devices reach, and no downstream receives a request.
#[tokio::test]
async fn a_deployer_device_reaches_no_other_route() {
    let harness = TestDatabase::create().await.expect("a test database");
    deployer(harness.pool()).await;
    let (sites, sites_seen) = stub("/v1/sites").await;
    let (notes, notes_seen) = stub("/v1/notes").await;
    let app = app(&harness, sites, notes);

    let manifest = format!("/v1/sites/{SITE}/manifest");
    for (method, path) in [
        ("PUT", manifest.as_str()),
        ("GET", "/v1/notes/connections"),
        ("GET", "/v1/operations"),
    ] {
        assert_eq!(
            call(&app, method, path, DEPLOYER, b"").await,
            (403, "platform.auth.forbidden".to_owned()),
            "{method} {path} with a site_deployer session"
        );
    }
    assert_eq!(
        count(&sites_seen),
        0,
        "the sites service received a request"
    );
    assert_eq!(
        count(&notes_seen),
        0,
        "the notes service received a request"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-route-classes`, "A deployer device of the site": the upload of a `site_deployer`
/// paired for [`SITE`] reaches the stub sites service once, with its body unchanged and with
/// minted claims that name the device kind and the site.
#[tokio::test]
async fn a_deployer_device_deploys_its_site() {
    let harness = TestDatabase::create().await.expect("a test database");
    let device_id = deployer(harness.pool()).await;
    let (sites, seen) = stub("/v1/sites").await;
    let (notes, _) = stub("/v1/notes").await;
    let app = app(&harness, sites, notes);

    assert_eq!(
        call(&app, "POST", &deploys(SITE), DEPLOYER, BUNDLE).await.0,
        200,
        "the upload is served"
    );
    let seen = seen.lock().expect("the record").clone();
    assert_eq!(seen.len(), 1, "one downstream request");
    let (path, headers, body) = &seen[0];
    assert_eq!(path, &deploys(SITE));
    assert_eq!(body.as_slice(), BUNDLE, "the bundle passes unchanged");
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    assert_eq!(header("content-type"), Some("application/x-tar"));
    assert_eq!(header("x-ratatoskr-session-kind"), Some("device"));
    assert_eq!(
        header("x-ratatoskr-device-id"),
        Some(device_id.to_string().as_str())
    );
    assert_eq!(header("x-ratatoskr-device-kind"), Some("site_deployer"));
    assert_eq!(
        header("x-ratatoskr-site-id"),
        Some(SITE),
        "the minted claims name the site of the device"
    );
    assert_eq!(header("authorization"), None, "no credential is forwarded");

    harness.cleanup().await.expect("cleanup");
}
