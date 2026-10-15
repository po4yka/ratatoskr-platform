//! What the tests of the site owner session need (workspace change
//! `add-site-owner-editing-and-live`): Edge over a test database with a stub sites service, the
//! owner's primary session, an approved site, and the sign-in through the two routes.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    dead_code,
    reason = "assertions in a test binary; each test binary uses a part of this module"
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
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

pub(crate) const AUDIENCE: &str = "edge";
pub(crate) const OWNER: &str = "site-owner-tests-owner-credential";
pub(crate) const SITE: &str = "site-a";
pub(crate) const OTHER_SITE: &str = "site-b";
pub(crate) const HOSTNAME: &str = "garden.example.org";
pub(crate) const OTHER_HOSTNAME: &str = "orchard.example.org";
pub(crate) const VERIFIER: &str = "a-pkce-verifier-of-the-site-0000000000000000000";
pub(crate) const COOKIE: &str = "__Host-ratatoskr-site";

pub(crate) fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// One request that a stub received.
#[derive(Debug, Clone)]
pub(crate) struct Received1 {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

/// A stub sites service. `GET /internal/sites/{site}/manifest` answers the active manifest of
/// [`Stub::manifests`]; a path in [`Stub::answers`] answers its status and body; every other
/// request answers `200 {}`. Every request but the manifest read is recorded.
#[derive(Clone)]
pub(crate) struct Stub {
    pub(crate) address: std::net::SocketAddr,
    pub(crate) seen: Arc<Mutex<Vec<Received1>>>,
    pub(crate) manifests: Arc<Mutex<BTreeMap<String, Value>>>,
    pub(crate) answers: Arc<Mutex<BTreeMap<String, (u16, Value)>>>,
}

impl Stub {
    pub(crate) async fn start() -> Self {
        let seen: Arc<Mutex<Vec<Received1>>> = Arc::default();
        let manifests: Arc<Mutex<BTreeMap<String, Value>>> = Arc::default();
        let answers: Arc<Mutex<BTreeMap<String, (u16, Value)>>> = Arc::default();
        let (record, active, fixed) = (
            Arc::clone(&seen),
            Arc::clone(&manifests),
            Arc::clone(&answers),
        );
        let handler = any(move |request: Received| {
            let (record, active, fixed) =
                (Arc::clone(&record), Arc::clone(&active), Arc::clone(&fixed));
            async move {
                let (parts, body) = request.into_parts();
                let path = parts.uri.path().to_owned();
                if let Some(site) = path
                    .strip_prefix("/internal/sites/")
                    .and_then(|rest| rest.strip_suffix("/manifest"))
                {
                    return match active.lock().expect("the manifests").get(site) {
                        Some(manifest) => answer(200, manifest),
                        None => answer(
                            404,
                            &json!({"code": "sites.site_not_found",
                            "message": "No such site.", "retryable": false}),
                        ),
                    };
                }
                let bytes = body.collect().await.expect("a body").to_bytes().to_vec();
                record.lock().expect("the record").push(Received1 {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body: bytes,
                });
                match fixed.lock().expect("the answers").get(&path) {
                    Some((status, body)) => answer(*status, body),
                    None => answer(200, &json!({})),
                }
            }
        });
        let router = Router::new().route("/{*rest}", handler);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener");
        let address = listener.local_addr().expect("a listener address");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("the stub serves");
        });
        Self {
            address,
            seen,
            manifests,
            answers,
        }
    }

    /// The active manifest of `site` that the stub answers.
    pub(crate) fn manifest(&self, site: &str, hostname: &str, capabilities: &[&str]) {
        self.manifests.lock().expect("the manifests").insert(
            site.to_owned(),
            json!({
                "site_id": site,
                "manifest_sha256": "a".repeat(64),
                "hostname": hostname,
                "publications": ["garden"],
                "capabilities": capabilities,
            }),
        );
    }

    pub(crate) fn count(&self) -> usize {
        self.seen.lock().expect("the record").len()
    }

    pub(crate) fn received(&self) -> Vec<Received1> {
        self.seen.lock().expect("the record").clone()
    }
}

fn answer(status: u16, body: &Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a stub answer")
}

/// A gateway route to `listener`.
pub(crate) fn route(prefix: &str, listener: std::net::SocketAddr) -> GatewayRouteConfig {
    GatewayRouteConfig {
        prefix: prefix.to_owned(),
        listener,
        class: Some(GatewayRouteClass::Control),
        capabilities_path: "/v1/capabilities".to_owned(),
        archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
    }
}

/// The API state over the test database with the sites route to `sites` and the `extra` routes.
pub(crate) fn state(
    harness: &TestDatabase,
    sites: std::net::SocketAddr,
    extra: &[(&str, &str, std::net::SocketAddr)],
) -> ApiState {
    let mut routes = BTreeMap::from([("sites".to_owned(), route("/v1/sites", sites))]);
    for (name, prefix, listener) in extra {
        routes.insert((*name).to_owned(), route(prefix, *listener));
    }
    let gateway = GatewayConfig {
        routes,
        ..GatewayConfig::default()
    };
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let mut state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
    state.gateway = platform_public_api::gateway::Gateway::from_config(&gateway);
    state
}

/// Edge's public router over `state`.
pub(crate) fn app(state: ApiState) -> Router {
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

/// A primary browser session with `credential`, and the live owner grant when `owner`.
pub(crate) async fn primary(pool: &sqlx::PgPool, credential: &str, owner: bool) -> uuid::Uuid {
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

/// Store the approval of `site` for `hostname` with `capabilities`, in `state`.
pub(crate) async fn approval(
    pool: &sqlx::PgPool,
    owner: uuid::Uuid,
    site: &str,
    hostname: &str,
    capabilities: &[&str],
    state: &str,
) {
    let capabilities: Vec<String> = capabilities.iter().map(|c| (*c).to_owned()).collect();
    sqlx::query(
        "insert into identity.site_approvals
             (site_id, changed_by, manifest_sha256, capabilities, hostname, state, changed_at)
         values ($1, $2, $3, $4, $5, $6, now())
         on conflict (site_id) do update
            set capabilities = excluded.capabilities, hostname = excluded.hostname,
                state = excluded.state",
    )
    .bind(site)
    .bind(owner)
    .bind("a".repeat(64))
    .bind(capabilities)
    .bind(hostname)
    .bind(state)
    .execute(pool)
    .await
    .expect("an approval");
}

/// The status, the headers and the JSON body of one request.
pub(crate) async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The status and the error code of one request.
pub(crate) async fn status_and_code(app: &Router, request: Request<Body>) -> (u16, String) {
    let (status, _, body) = send(app, request).await;
    (
        status.as_u16(),
        body["code"].as_str().unwrap_or_default().to_owned(),
    )
}

/// `POST /v1/sites/{site}/sign-in-codes` with the owner's bearer: the status and the body.
pub(crate) async fn request_code(app: &Router, site: &str) -> (StatusCode, Value) {
    let body = json!({
        "challenge": platform_identity::site_sign_in::challenge_of(VERIFIER),
        "return_path": "/notes/start",
    });
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/sites/{site}/sign-in-codes"))
        .header("authorization", format!("Bearer {OWNER}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request");
    let (status, _, body) = send(app, request).await;
    (status, body)
}

/// `POST /v1/sites/{site}/sessions` from the site origin `origin`.
pub(crate) async fn redeem(
    app: &Router,
    site: &str,
    code: &str,
    origin: &str,
) -> (StatusCode, HeaderMap, Value) {
    let body = json!({"code": code, "verifier": VERIFIER});
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/sites/{site}/sessions"))
        .header("content-type", "application/json")
        .header("origin", origin)
        .header("x-ratatoskr-site-request", "1")
        .body(Body::from(body.to_string()))
        .expect("a request");
    send(app, request).await
}

/// Sign in to `site` (approved for `hostname`) through both routes, and answer the value of the
/// session cookie.
pub(crate) async fn sign_in(app: &Router, site: &str, hostname: &str) -> String {
    let (status, issued) = request_code(app, site).await;
    assert_eq!(status, StatusCode::CREATED, "the sign-in code: {issued}");
    let code = issued["code"].as_str().expect("a code");
    let (status, headers, body) = redeem(app, site, code, &format!("https://{hostname}")).await;
    assert_eq!(status, StatusCode::CREATED, "the redeem: {body}");
    cookie_value(&headers).expect("a session cookie")
}

/// The value of the session cookie that a response sets.
pub(crate) fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            cookie
                .strip_prefix(&format!("{COOKIE}="))
                .map(|rest| rest.split(';').next().unwrap_or_default().to_owned())
        })
}

/// A request of the site's page with the session cookie: an unsafe method carries the site
/// request header when `site_header`, and `Origin` when `origin` is given.
pub(crate) fn from_site(
    method: &str,
    uri: &str,
    cookie: &str,
    site_header: bool,
    origin: Option<&str>,
    body: &Value,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("{COOKIE}={cookie}"))
        .header("content-type", "application/json")
        .header("idempotency-key", uuid::Uuid::now_v7().to_string());
    if site_header {
        builder = builder.header("x-ratatoskr-site-request", "1");
    }
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    builder.body(body).expect("a request")
}

/// The payloads of the outbox rows of `subject`.
pub(crate) async fn outbox(pool: &sqlx::PgPool, subject: &str) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "select payload from operations.outbox where subject = $1 order by enqueued_at",
    )
    .bind(subject)
    .fetch_all(pool)
    .await
    .expect("the outbox")
}
