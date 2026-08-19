//! `GET /v2/capabilities` — tests P-1 … P-6.
//!
//! ADR-0008 says a capability is reported when its deployment requirement is configured, the
//! components that requirement names are healthy, and the caller is authorized for it. These tests
//! are that sentence, one clause at a time, plus the drift gate that keeps the vocabulary honest:
//! every name this build can print must be a name this build serves a route for.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::config::PublicConfig;
use platform_core::{Capability, RuntimeRole};
use platform_http::{HttpState, RuntimeState};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tower::ServiceExt as _;

const CREDENTIAL: &str = "capabilities-credential-000000000000";
const AUDIENCE: &str = "edge";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

/// The real public pipeline, so a refusal is rendered by the same middleware production uses.
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
        platform_public_api::routes(std::sync::Arc::new(state)),
    )
}

/// A deployment: `database_reachable` is what the readiness prober last found, `bus` whether one is
/// configured at all.
fn deployment(harness: &TestDatabase, database_reachable: bool, bus: bool) -> ApiState {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(database_reachable);
    ApiState::new(harness.database.clone(), AUDIENCE, health, bus)
}

async fn seed(pool: &sqlx::PgPool, credential: &str) {
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
}

fn ask(credential: Option<&str>) -> Request<Body> {
    let mut request = Request::builder().method("GET").uri("/v2/capabilities");
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    request.body(Body::empty()).expect("a request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// The names in a capability document.
fn names(body: &serde_json::Value) -> Vec<String> {
    body["capabilities"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|value| value.as_str().expect("a string").to_owned())
        .collect()
}

/// P-1. The response is exactly the three members `ARCHITECTURE.md` S12 fixes, and no fourth.
///
/// The shape is a contract with every client, so a member added by accident — a health field, a
/// service name, a count — is a leak or a promise, and both are worth failing a build over.
#[tokio::test]
async fn the_document_is_the_shape_s12_fixes_and_nothing_more() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL).await;
    let app = app(deployment(&harness, true, true));

    let (status, body) = send(&app, ask(Some(CREDENTIAL))).await;

    assert_eq!(status, StatusCode::OK);
    let object = body.as_object().expect("an object");
    let mut members: Vec<&str> = object.keys().map(String::as_str).collect();
    members.sort_unstable();
    assert_eq!(
        members,
        ["api_version", "capabilities", "minimum_client_versions"],
        "S12 fixes these three members: {body}"
    );
    assert_eq!(body["api_version"], "2.0");
    assert_eq!(body["minimum_client_versions"]["web"], "2.0");
    assert_eq!(body["minimum_client_versions"]["mobile"], "2.0");
}

/// P-2. A whole deployment reports the capability it can honour.
#[tokio::test]
async fn a_whole_deployment_reports_content_submit() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL).await;
    let app = app(deployment(&harness, true, true));

    let (status, body) = send(&app, ask(Some(CREDENTIAL))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(names(&body), vec![Capability::ContentSubmit.as_str()]);
}

/// P-3. No bus, no `content.submit`.
///
/// The subtle case, and the reason the requirement is not simply "a database": a capture IS
/// accepted durably without a bus — the outbox is the durable half — but nothing ever publishes the
/// command, so the operation never progresses. Reporting the capability would tell a client a
/// feature works when from its side it does not.
#[tokio::test]
async fn without_a_bus_the_capability_is_absent_even_though_the_route_answers() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL).await;
    let app = app(deployment(&harness, true, false));

    let (status, body) = send(&app, ask(Some(CREDENTIAL))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(names(&body).is_empty(), "{body}");

    // And the route it names is still there, which is exactly the discrepancy the document exists
    // to describe: the request is accepted, the work will not happen.
    let capture = Request::builder()
        .method("POST")
        .uri("/v2/captures")
        .header("authorization", format!("Bearer {CREDENTIAL}"))
        .header("idempotency-key", "p3")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"url":"https://example.test/a"}"#))
        .expect("a request");
    let (status, _) = send(&app, capture).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// P-4. A database the prober cannot reach removes the capability, and the fact is the one
/// `/health/ready` publishes rather than a second opinion.
#[tokio::test]
async fn an_unreachable_database_removes_the_capability() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL).await;
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let app = app(ApiState::new(
        harness.database.clone(),
        AUDIENCE,
        Arc::clone(&health),
        true,
    ));

    let (_, before) = send(&app, ask(Some(CREDENTIAL))).await;
    assert_eq!(names(&before), vec![Capability::ContentSubmit.as_str()]);

    // The prober's next pass finds nothing. Same process, same state object.
    health.set_database_reachable(false);

    let (status, after) = send(&app, ask(Some(CREDENTIAL))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(names(&after).is_empty(), "{after}");
}

/// P-5. The route authenticates like every other `/v2` route, and refuses identically.
#[tokio::test]
async fn the_route_is_authenticated() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL).await;
    let app = app(deployment(&harness, true, true));

    for credential in [None, Some("not-a-credential")] {
        let (status, body) = send(&app, ask(credential)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{credential:?}");
        assert_eq!(
            body["code"], "platform.auth.unauthenticated",
            "a missing credential and a wrong one must be indistinguishable: {body}"
        );
    }
}

/// P-6. The drift gate, in the direction that matters: every name this build can print is a name
/// this build serves a route for.
///
/// A capability whose route does not exist is a button that 404s. Adding a variant to the
/// vocabulary therefore fails here until the route family that implements it is served, which is
/// what ADR-0008 means by "a name on this list is a promise the route tree has to keep".
#[test]
fn every_capability_names_a_route_this_build_serves() {
    let served: Vec<&str> = platform_public_api::surface()
        .routes
        .iter()
        .map(|route| route.path)
        .collect();

    for capability in Capability::ALL {
        let required = match capability {
            Capability::ContentSubmit => "/v2/captures",
            Capability::TelegramMiniApp => "/v2/sessions/telegram",
            // A new variant arrives here and fails until somebody names the route it promises,
            // which is the point: the vocabulary may not grow ahead of the route tree.
            _ => panic!("{capability} names no route in this test; add the one it promises"),
        };
        assert!(
            served.contains(&required),
            "{capability} promises {required}, which nothing serves: {served:?}"
        );
    }
}

/// P-7. The array is sorted, so two consecutive responses from an unchanged deployment are
/// byte-identical and a client may compare them.
#[test]
fn the_vocabulary_is_sorted() {
    let names: Vec<&str> = Capability::ALL.iter().map(|c| c.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "Capability::ALL must be in wire order");
}

/// P-8. Every path the generated document promises is a path this router actually serves.
///
/// The other direction of the drift gate, and the one a structural argument cannot make on its
/// own: the route table produces both the router and the document, so they agree by construction —
/// but only if the table's paths are the strings `axum` matches on. This sends a real request to
/// each and refuses to accept the router's own "no route matched".
#[tokio::test]
async fn every_documented_path_is_served() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(deployment(&harness, true, true));

    for route in platform_public_api::surface().routes {
        let path = route
            .path
            .replace("{operation_id}", "01a018ae-b4e5-7f90-a17f-1e60c8ce61be")
            .replace("{relay_id}", "01a018ae-b4e5-7f90-a17f-1e60c8ce61be")
            .replace("{provider}", "github");
        let method = match route.method {
            platform_api_doc::Method::Get => "GET",
            platform_api_doc::Method::Post => "POST",
        };
        let request = Request::builder()
            .method(method)
            .uri(&path)
            .body(Body::empty())
            .expect("a request");
        let (status, body) = send(&app, request).await;

        // What this rules out is the ROUTER's own two failures. A route that authenticates answers
        // 401 without a credential; a public one gets far enough to complain about the request
        // itself, which is equally proof that something is serving the path.
        assert_ne!(
            body["code"], "platform.route.not_found",
            "{method} {path} is documented and nothing serves it"
        );
        assert_ne!(
            body["code"], "platform.route.method_not_allowed",
            "{method} {path} is documented under the wrong method"
        );
        if route.security == platform_api_doc::Security::None {
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{method} {path} is public, so an empty request is a CLIENT error: {body}"
            );
        } else {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
            assert_eq!(
                body["code"], "platform.auth.unauthenticated",
                "{method} {path}"
            );
        }
    }
}
