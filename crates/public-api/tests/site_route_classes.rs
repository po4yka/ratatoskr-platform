//! The class of every route under `/v1/sites/{site}/` (ADR-0017; workspace change
//! `add-site-publications`, spec `site-route-classes`).

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
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{GatewayConfig, GatewayRouteClass, GatewayRouteConfig, PublicConfig};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::sites::{ROUTES, SiteRoute, SiteRouteClass, check_routes};
use platform_public_api::{ApiState, auth};
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const AUDIENCE: &str = "edge";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// The class table of the spec, as `(methods, path below /v1/sites, class)`. `GET /v1/sites` and
/// `GET .../manifest` are the two reads of the web client (workspace task 6.14).
fn listed() -> Vec<(Vec<Method>, &'static str, SiteRouteClass)> {
    use SiteRouteClass::{Deploy, Management, Release, SignIn, SiteOrigin};
    let read_write = vec![Method::GET, Method::PUT, Method::DELETE];
    vec![
        (vec![Method::GET], "", Management),
        (
            vec![Method::GET, Method::PUT],
            "/{site}/manifest",
            Management,
        ),
        (
            vec![Method::POST, Method::DELETE],
            "/{site}/manifest/approval",
            Management,
        ),
        (read_write.clone(), "/{site}/publications", Management),
        (read_write, "/{site}/publications/{publication}", Management),
        (vec![Method::GET], "/{site}/releases", Management),
        (vec![Method::POST], "/{site}/releases", Release),
        (vec![Method::POST], "/{site}/sign-in-codes", Management),
        (vec![Method::POST], "/{site}/sessions", SignIn),
        (vec![Method::POST], "/{site}/deploys", Deploy),
        (
            vec![Method::POST],
            "/{site}/deploys/{commit}/activate",
            Deploy,
        ),
        (
            vec![Method::GET, Method::HEAD],
            "/{site}/content/{*rest}",
            SiteOrigin,
        ),
    ]
}

/// Spec `site-route-classes`, "A route without a class stops Edge": the start-up check refuses a
/// table with an unclassified route and names it, and accepts the shipped table.
#[test]
fn unclassified_site_route_fails_start_up() {
    const UNCLASSIFIED: [SiteRoute; 1] = [SiteRoute {
        methods: &[Method::PUT],
        path: "/{site}/manifest",
        class: None,
    }];
    let refusal = check_routes(&UNCLASSIFIED);
    assert!(
        refusal
            .as_ref()
            .is_err_and(|reason| reason.contains("PUT /v1/sites/{site}/manifest")),
        "a route without a class must stop Edge and be named: {refusal:?}"
    );
    assert_eq!(check_routes(ROUTES), Ok(()), "the shipped table starts");
}

/// Spec `site-route-classes`, "The route table matches the class list": every route of the list
/// has exactly its class, and the table holds no other route.
#[test]
fn site_routes_have_their_listed_classes() {
    let listed = listed();
    for (methods, path, class) in &listed {
        for method in methods {
            let rows: Vec<Option<SiteRouteClass>> = ROUTES
                .iter()
                .filter(|row| row.path == *path && row.methods.contains(method))
                .map(|row| row.class)
                .collect();
            assert_eq!(
                rows,
                [Some(*class)],
                "{method} /v1/sites{path} must have exactly the class {class:?}"
            );
        }
    }
    for row in ROUTES {
        for method in row.methods {
            assert!(
                listed
                    .iter()
                    .any(|(methods, path, _)| *path == row.path && methods.contains(method)),
                "{method} /v1/sites{} is not in the class list",
                row.path
            );
        }
    }
}

/// Spec `site-route-classes`, "The deploy routes are not management routes": the upload and the
/// activation of a deploy have exactly the class `deploy`.
#[test]
fn deploy_routes_have_the_deploy_class() {
    for path in ["/{site}/deploys", "/{site}/deploys/{commit}/activate"] {
        let rows: Vec<Option<SiteRouteClass>> = ROUTES
            .iter()
            .filter(|row| row.path == path && row.methods.contains(&Method::POST))
            .map(|row| row.class)
            .collect();
        assert_eq!(
            rows,
            [Some(SiteRouteClass::Deploy)],
            "POST /v1/sites{path} must have exactly the class Deploy, and not Management"
        );
    }
}

/// A session of `kind` for a new user, optionally with the owner grant.
async fn session(
    pool: &sqlx::PgPool,
    credential: &str,
    kind: SessionKind,
    owner: bool,
) -> uuid::Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let device_id = if kind == SessionKind::Device {
        Some(
            platform_identity::device::register_device(
                pool,
                user.user_id,
                platform_identity::DeviceKind::ExportAgent,
                None,
                platform_identity::SecretDigest::of("site-classes-device-secret"),
                now(),
            )
            .await
            .expect("a device")
            .device_id,
        )
    } else {
        None
    };
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind,
            device_id,
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

/// A stub sites service that counts the requests it receives and answers each with `200`.
async fn stub_sites() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let handler = any(move || {
        let seen = Arc::clone(&seen);
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }
    });
    let router = Router::new()
        .route("/v1/sites", handler.clone())
        .route("/v1/sites/{*rest}", handler);
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

async fn call(app: &Router, method: &str, path: &str, credential: Option<&str>) -> (u16, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from("{}")).expect("a request"))
        .await
        .expect("a response");
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

/// Spec `site-route-classes`, "A session without the owner grant manages a site": a management
/// route is served only to a primary session with the live owner grant, and every other caller is
/// refused before the sites service.
#[tokio::test]
async fn management_route_needs_live_owner_grant() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    session(
        pool,
        "site-classes-member-0000000000",
        SessionKind::Browser,
        false,
    )
    .await;
    let revoked = session(
        pool,
        "site-classes-revoked-000000000",
        SessionKind::Browser,
        true,
    )
    .await;
    platform_identity::grant::revoke(pool, revoked, PLATFORM_OWNER_GRANT, now())
        .await
        .expect("the grant is revoked");
    session(
        pool,
        "site-classes-device-0000000000",
        SessionKind::Device,
        true,
    )
    .await;
    session(
        pool,
        "site-classes-owner-00000000000",
        SessionKind::Browser,
        true,
    )
    .await;
    let (listener, count) = stub_sites().await;
    let app = app(&harness, listener);
    let manifest = "/v1/sites/demo-site/manifest";

    for (label, credential, status, code) in [
        ("no credential", None, 401, "platform.auth.unauthenticated"),
        (
            "a session without the grant",
            Some("site-classes-member-0000000000"),
            403,
            "platform.auth.forbidden",
        ),
        (
            "a session whose grant was revoked",
            Some("site-classes-revoked-000000000"),
            403,
            "platform.auth.forbidden",
        ),
        (
            "a device session of the owner",
            Some("site-classes-device-0000000000"),
            403,
            "platform.auth.forbidden",
        ),
    ] {
        assert_eq!(
            call(&app, "PUT", manifest, credential).await,
            (status, code.to_owned()),
            "PUT {manifest} with {label}"
        );
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "a refused management request reached the sites service"
    );

    let owner = Some("site-classes-owner-00000000000");
    assert_eq!(call(&app, "PUT", manifest, owner).await.0, 200);
    assert_eq!(count.load(Ordering::SeqCst), 1, "the owner reaches sites");

    // Workspace task 6.14: the two reads of the web client are management routes too.
    for path in ["/v1/sites", manifest] {
        assert_eq!(
            call(&app, "GET", path, Some("site-classes-member-0000000000")).await,
            (403, "platform.auth.forbidden".to_owned()),
            "GET {path} with a session without the grant"
        );
        assert_eq!(
            call(&app, "GET", path, owner).await.0,
            200,
            "GET {path} for the owner"
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 3, "both reads reach sites");

    assert_eq!(
        call(&app, "GET", "/v1/sites/demo-site/unlisted", owner).await,
        (404, "platform.route.not_found".to_owned()),
        "a path that no row holds is not proxied"
    );
    assert_eq!(
        call(&app, "PATCH", manifest, owner).await,
        (405, "platform.route.method_not_allowed".to_owned()),
        "a method that the row does not hold is not proxied"
    );
    assert_eq!(count.load(Ordering::SeqCst), 3);

    harness.cleanup().await.expect("cleanup");
}
