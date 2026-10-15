//! The owner's library and knowledge search from a site (workspace change
//! `add-site-owner-editing-and-live`, spec `site-system-data-access`), against a stub Knowledge.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in a test binary"
)]

mod site_owner_support;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::Request;
use platform_persistence::test_support::TestDatabase;
use serde_json::{Value, json};
use site_owner_support::{
    HOSTNAME, OWNER, SITE, Stub, approval, from_site, primary, send, sign_in,
};
use tokio::net::TcpListener;

const CAPABILITIES: &[&str] = &["pages.read", "library.read", "knowledge.search"];

fn knowledge_page() -> Value {
    json!({
        "results": [
            {"analysis_id": "018f0000-0000-7000-8000-000000000a01",
             "document_id": "018f0000-0000-7000-8000-000000000a02",
             "title": "Durable queues", "snippet": "a durable queue", "rank": 0.9,
             "read_state": "unread"},
            {"analysis_id": "018f0000-0000-7000-8000-000000000a03",
             "document_id": "018f0000-0000-7000-8000-000000000a04",
             "title": "Durable logs", "snippet": "a durable log", "rank": 0.5,
             "read_state": "read"},
        ],
        "has_more": false,
    })
}

/// An Edge with `site-a` approved and active, a Knowledge at `knowledge`, and a site owner cookie.
async fn edge(harness: &TestDatabase, knowledge: std::net::SocketAddr) -> (Router, String) {
    let sites = Stub::start().await;
    sites.manifest(SITE, HOSTNAME, CAPABILITIES);
    let owner = primary(harness.pool(), OWNER, true).await;
    approval(
        harness.pool(),
        owner,
        SITE,
        HOSTNAME,
        CAPABILITIES,
        "approved",
    )
    .await;
    let mut state =
        site_owner_support::state(harness, sites.address, &[("knowledge", "/v1/k", knowledge)]);
    state.gateway = {
        let mut config = platform_core::config::GatewayConfig {
            routes: [
                (
                    "sites".to_owned(),
                    site_owner_support::route("/v1/sites", sites.address),
                ),
                (
                    "knowledge".to_owned(),
                    site_owner_support::route("/v1/k", knowledge),
                ),
            ]
            .into_iter()
            .collect(),
            ..platform_core::config::GatewayConfig::default()
        };
        config.budgets.control.response_timeout_seconds = 1;
        platform_public_api::gateway::Gateway::from_config(&config)
    };
    let app = site_owner_support::app(state);
    let cookie = sign_in(&app, SITE, HOSTNAME).await;
    (app, cookie)
}

/// Spec `site-system-data-access`, "The same query through both routes".
#[tokio::test]
async fn site_library_equals_primary_library() {
    let harness = TestDatabase::create().await.expect("a test database");
    let knowledge = Stub::start().await;
    knowledge
        .answers
        .lock()
        .unwrap()
        .insert("/internal/search".to_owned(), (200, knowledge_page()));
    let (app, cookie) = edge(&harness, knowledge.address).await;

    let query = "q=durable&limit=5&offset=0";
    let primary_request = Request::builder()
        .uri(format!("/v1/library/search?{query}"))
        .header("authorization", format!("Bearer {OWNER}"))
        .body(Body::empty())
        .expect("a request");
    let (status, _, primary_page) = send(&app, primary_request).await;
    assert_eq!(status, 200, "{primary_page}");
    let request = from_site(
        "GET",
        &format!("/v1/sites/site-a/library?{query}"),
        &cookie,
        false,
        None,
        &Value::Null,
    );
    let (status, _, site_page) = send(&app, request).await;
    assert_eq!(status, 200, "{site_page}");
    assert_eq!(
        site_page, primary_page,
        "the same items, order and read state"
    );
    let tenants: Vec<String> = knowledge
        .received()
        .iter()
        .map(|request| request.uri.clone())
        .collect();
    assert_eq!(tenants.len(), 2, "{tenants:?}");
    assert_eq!(
        tenants[0], tenants[1],
        "both routes ask Knowledge the same question"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-system-data-access`, "A tenant selector".
#[tokio::test]
async fn tenant_selector_is_invalid_before_knowledge() {
    let harness = TestDatabase::create().await.expect("a test database");
    let knowledge = Stub::start().await;
    let (app, cookie) = edge(&harness, knowledge.address).await;

    for path in [
        "/v1/sites/site-a/library",
        "/v1/sites/site-a/knowledge/search",
    ] {
        let request = from_site(
            "GET",
            &format!("{path}?q=x&tenant=user:someone"),
            &cookie,
            false,
            None,
            &Value::Null,
        );
        let (status, _, body) = send(&app, request).await;
        assert_eq!(
            (status.as_u16(), body["code"].as_str()),
            (400, Some("platform.request.invalid")),
            "{path}: {body}"
        );
    }
    assert_eq!(knowledge.count(), 0, "Knowledge received a query");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-system-data-access`, "Knowledge is unavailable": a Knowledge that does not answer
/// within its budget gives the retryable error and no partial result.
#[tokio::test]
async fn knowledge_timeout_is_retryable_unavailable() {
    let harness = TestDatabase::create().await.expect("a test database");
    let slow = Router::new().route(
        "/internal/search",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            axum::Json(knowledge_page())
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, slow).await.expect("the slow stub") });
    let (app, cookie) = edge(&harness, address).await;

    for path in [
        "/v1/sites/site-a/library",
        "/v1/sites/site-a/knowledge/search",
    ] {
        let request = from_site(
            "GET",
            &format!("{path}?q=durable"),
            &cookie,
            false,
            None,
            &Value::Null,
        );
        let (status, _, body) = send(&app, request).await;
        assert_eq!(
            (status.as_u16(), body["retryable"].as_bool()),
            (504, Some(true)),
            "{path}: {body}"
        );
        assert!(body.get("items").is_none(), "a partial result: {body}");
    }

    harness.cleanup().await.expect("cleanup");
}

/// Review L10 of SITE-027: the knowledge search needs a query of 1 to 512 characters; without one
/// it is refused before Knowledge.
#[tokio::test]
async fn knowledge_search_requires_a_query() {
    let harness = TestDatabase::create().await.expect("a test database");
    let knowledge = Stub::start().await;
    let (app, cookie) = edge(&harness, knowledge.address).await;

    for query in ["", "?q=", "?limit=5"] {
        let request = from_site(
            "GET",
            &format!("/v1/sites/site-a/knowledge/search{query}"),
            &cookie,
            false,
            None,
            &Value::Null,
        );
        let (status, _, body) = send(&app, request).await;
        assert_eq!(
            (status.as_u16(), body["code"].as_str()),
            (400, Some("platform.request.invalid")),
            "the search {query:?}: {body}"
        );
    }
    assert_eq!(knowledge.count(), 0, "Knowledge received a query");

    harness.cleanup().await.expect("cleanup");
}
