//! The capability check of the site-origin routes (workspace change
//! `add-site-owner-editing-and-live`, spec `site-capability-enforcement`): each route names one
//! capability, and Edge checks it against the site's effective set before it forwards or reads.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod site_owner_support;

use axum::body::Body;
use http::{Method, Request};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::sites::{ROUTES, SiteRoute, SiteRouteClass, check_routes};
use serde_json::{Value, json};
use site_owner_support::{
    HOSTNAME, OWNER, SITE, Stub, app, approval, from_site, outbox, primary, sign_in, state,
    status_and_code,
};

const WRITE: &str = "cmd.notes.file.write_requested.v1";

/// Spec `site-capability-enforcement`, "A site-origin route without a capability": the start-up
/// check refuses it and names it, and every shipped site-origin row names one capability.
#[test]
fn site_origin_route_names_one_capability() {
    const NAMELESS: [SiteRoute; 1] = [SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/library",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: None,
    }];
    let refusal = check_routes(&NAMELESS);
    assert!(
        refusal
            .as_ref()
            .is_err_and(|reason| reason.contains("GET /v1/sites/{site}/library")),
        "a site-origin route without a capability must stop Edge and be named: {refusal:?}"
    );
    for row in ROUTES {
        let site_origin = row.class == Some(SiteRouteClass::SiteOrigin);
        assert_eq!(
            row.capability.is_some(),
            site_origin,
            "/v1/sites{} names a capability exactly when it is site-origin",
            row.path
        );
    }
    assert_eq!(check_routes(ROUTES), Ok(()), "the shipped table starts");
}

/// An Edge whose `site-a` is approved and active with `capabilities`, with a recording sites stub
/// and a recording Knowledge stub. Answers the router, the sites stub and the Knowledge stub.
async fn edge(harness: &TestDatabase, capabilities: &[&str]) -> (axum::Router, Stub, Stub) {
    let sites = Stub::start().await;
    let knowledge = Stub::start().await;
    sites.manifest(SITE, HOSTNAME, capabilities);
    let owner = primary(harness.pool(), OWNER, true).await;
    approval(
        harness.pool(),
        owner,
        SITE,
        HOSTNAME,
        capabilities,
        "approved",
    )
    .await;
    let app = app(state(
        harness,
        sites.address,
        &[("knowledge", "/v1/k", knowledge.address)],
    ));
    (app, sites, knowledge)
}

fn anonymous(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

/// Spec `site-capability-enforcement`, "A public capability that was not requested".
#[tokio::test]
async fn unrequested_public_capability_is_site_not_found() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, sites, _) = edge(&harness, &["pages.read"]).await;

    assert_eq!(
        status_and_code(&app, anonymous("/v1/sites/site-a/content/search?q=garden")).await,
        (404, "sites.site_not_found".to_owned())
    );
    assert_eq!(
        status_and_code(&app, anonymous("/v1/sites/site-unknown/content/search?q=x")).await,
        (404, "sites.site_not_found".to_owned()),
        "the same answer as for an unknown site"
    );
    assert_eq!(sites.count(), 0, "the sites service received a request");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-capability-enforcement`, "An owner-only capability that was not granted".
#[tokio::test]
async fn ungranted_owner_capability_is_forbidden() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, _, knowledge) = edge(&harness, &["pages.read", "draft.read"]).await;
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    let request = from_site(
        "GET",
        "/v1/sites/site-a/library?q=x",
        &cookie,
        false,
        None,
        &Value::Null,
    );
    assert_eq!(
        status_and_code(&app, request).await,
        (403, "edge.capability_not_granted".to_owned())
    );
    assert_eq!(knowledge.count(), 0, "Knowledge received a request");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-capability-enforcement`, "An anonymous caller asks for a draft".
#[tokio::test]
async fn owner_only_capability_refuses_anonymous() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, sites, _) = edge(&harness, &["pages.read", "draft.read"]).await;

    assert_eq!(
        status_and_code(&app, anonymous("/v1/sites/site-a/drafts/pages/home")).await,
        (401, "platform.auth.unauthenticated".to_owned())
    );
    assert_eq!(sites.count(), 0, "the sites service received a request");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-capability-enforcement`, "A withdrawn site": the owner session ends with the
/// approval, and an anonymous content request is `sites.site_not_found` at Edge.
#[tokio::test]
async fn withdrawn_site_is_site_not_found() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, sites, _) = edge(&harness, &["pages.read", "draft.read"]).await;
    let cookie = sign_in(&app, SITE, HOSTNAME).await;
    let request = Request::builder()
        .method("DELETE")
        .uri("/v1/sites/site-a/manifest/approval")
        .header("authorization", format!("Bearer {OWNER}"))
        .body(Body::empty())
        .expect("a request");
    assert_eq!(
        status_and_code(&app, request).await.0,
        200,
        "the withdrawal"
    );

    let request = from_site(
        "GET",
        "/v1/sites/site-a/drafts/pages/home",
        &cookie,
        false,
        None,
        &Value::Null,
    );
    assert_eq!(
        status_and_code(&app, request).await,
        (401, "platform.auth.unauthenticated".to_owned())
    );
    assert_eq!(
        status_and_code(&app, anonymous("/v1/sites/site-a/content/pages/home")).await,
        (404, "sites.site_not_found".to_owned())
    );
    assert_eq!(sites.count(), 0, "the sites service received a request");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-capability-enforcement`, "The sites service is down": the effective set cannot be
/// read, so an owner-only capability fails closed with the retryable unavailable error.
#[tokio::test]
async fn unknown_effective_set_fails_closed() {
    let harness = TestDatabase::create().await.expect("a test database");
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.local_addr().expect("an address")
    };
    let owner = primary(harness.pool(), OWNER, true).await;
    approval(
        harness.pool(),
        owner,
        SITE,
        HOSTNAME,
        &["pages.read", "notes.edit"],
        "approved",
    )
    .await;
    let app = app(state(&harness, closed, &[]));
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    let edit = json!({
        "source_ref": "src_0000000000000000",
        "base_hash": "b".repeat(64),
        "content_blob": {"owner": "notes", "id": "blob"},
        "content_hash": "c".repeat(64),
        "idempotency_key": "edit-while-sites-is-down",
    });
    let request = from_site(
        "POST",
        "/v1/sites/site-a/edits",
        &cookie,
        true,
        Some(&format!("https://{HOSTNAME}")),
        &edit,
    );
    let (status, _, body) = site_owner_support::send(&app, request).await;
    assert_eq!(
        (
            status.as_u16(),
            body["code"].as_str(),
            body["retryable"].as_bool()
        ),
        (503, Some("edge.upstream_unavailable"), Some(true)),
        "{body}"
    );
    assert!(
        outbox(harness.pool(), WRITE).await.is_empty(),
        "a command was written"
    );

    harness.cleanup().await.expect("cleanup");
}
