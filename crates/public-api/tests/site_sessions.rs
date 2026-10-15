//! The site sign-in and the site owner session at Edge (workspace change
//! `add-site-owner-editing-and-live`, spec `site-owner-session`), against a stub sites service.
//!
//! The release route is the route of the site owner session that this change has from its start
//! (the site-origin rows come with task 6.2), so the cookie, header and `Origin` rules are shown on
//! it.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod site_owner_support;

use axum::body::Body;
use http::{Request, StatusCode};
use serde_json::Value;
use site_owner_support::{
    COOKIE, HOSTNAME, OTHER_HOSTNAME, OWNER, SITE, Stub, app, approval, from_site, outbox, primary,
    redeem, request_code, send, sign_in, state, status_and_code,
};

const RELEASES: &str = "/v1/sites/site-a/releases";
const BUILD: &str = "cmd.sites.release.build_requested.v1";

/// An Edge with an approved [`SITE`] at [`HOSTNAME`], the owner's primary session, and the stub.
async fn edge(harness: &platform_persistence::test_support::TestDatabase) -> (axum::Router, Stub) {
    let stub = Stub::start().await;
    stub.manifest(SITE, HOSTNAME, &["pages.read", "notes.edit"]);
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
    (app(state(harness, stub.address, &[])), stub)
}

/// Spec `site-owner-session`, "The owner signs in to a site": one `site_owner` session, and a
/// host-only, strict, secure cookie.
#[tokio::test]
async fn redeem_sets_host_only_strict_cookie() {
    let harness = platform_persistence::test_support::TestDatabase::create()
        .await
        .expect("a test database");
    let (app, _) = edge(&harness).await;

    let (status, issued) = request_code(&app, SITE).await;
    assert_eq!(status, StatusCode::CREATED, "the sign-in code: {issued}");
    let code = issued["code"].as_str().expect("a code");
    assert_eq!(
        issued["redirect_url"].as_str(),
        Some(format!("https://{HOSTNAME}/.ratatoskr/sign-in/?code={code}").as_str()),
        "the callback page of the approved hostname"
    );
    let (status, headers, body) = redeem(&app, SITE, code, &format!("https://{HOSTNAME}")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["return_path"], "/notes/start");
    let cookie = headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|cookie| cookie.starts_with(&format!("{COOKIE}=")))
        .expect("the session cookie")
        .to_owned();
    let attributes: Vec<String> = cookie
        .split(';')
        .skip(1)
        .map(|attribute| attribute.trim().to_ascii_lowercase())
        .collect();
    for required in ["httponly", "secure", "samesite=strict", "path=/"] {
        assert!(
            attributes.iter().any(|attribute| attribute == required),
            "the cookie lacks {required}: {cookie}"
        );
    }
    assert!(
        !attributes
            .iter()
            .any(|attribute| attribute.starts_with("domain")),
        "the cookie names a domain: {cookie}"
    );
    let sessions: i64 = sqlx::query_scalar(
        "select count(*) from identity.sessions where kind = 'site_owner' and audience = $1",
    )
    .bind(format!("site:{SITE}"))
    .fetch_one(harness.pool())
    .await
    .expect("a count");
    assert_eq!(sessions, 1, "one site_owner session");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "The site is not approved": 404 `sites.site_not_found`, no code.
#[tokio::test]
async fn unapproved_site_issues_no_code() {
    let harness = platform_persistence::test_support::TestDatabase::create()
        .await
        .expect("a test database");
    let (app, _) = edge(&harness).await;
    let owner: uuid::Uuid = sqlx::query_scalar("select changed_by from identity.site_approvals")
        .fetch_one(harness.pool())
        .await
        .expect("the owner");
    approval(
        harness.pool(),
        owner,
        SITE,
        HOSTNAME,
        &["pages.read"],
        "withdrawn",
    )
    .await;

    let (status, body) = request_code(&app, SITE).await;
    assert_eq!(
        (status.as_u16(), body["code"].as_str()),
        (404, Some("sites.site_not_found")),
        "{body}"
    );
    let (status, body) = request_code(&app, "site-unknown").await;
    assert_eq!(
        (status.as_u16(), body["code"].as_str()),
        (404, Some("sites.site_not_found")),
        "an unknown site: {body}"
    );
    let codes: i64 = sqlx::query_scalar("select count(*) from identity.site_sign_in_codes")
        .fetch_one(harness.pool())
        .await
        .expect("a count");
    assert_eq!(codes, 0, "no code is issued");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "The owner session is sent as a bearer header".
#[tokio::test]
async fn bearer_header_is_refused() {
    let harness = platform_persistence::test_support::TestDatabase::create()
        .await
        .expect("a test database");
    let (app, _) = edge(&harness).await;
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    let request = Request::builder()
        .method("POST")
        .uri(RELEASES)
        .header("authorization", format!("Bearer {cookie}"))
        .header("x-ratatoskr-site-request", "1")
        .header("origin", format!("https://{HOSTNAME}"))
        .header("idempotency-key", "bearer-site-session")
        .body(Body::empty())
        .expect("a request");
    assert_eq!(
        status_and_code(&app, request).await,
        (401, "platform.auth.unauthenticated".to_owned())
    );
    assert!(outbox(harness.pool(), BUILD).await.is_empty());

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A cross-site form posts an edit", shown on the release route.
#[tokio::test]
async fn unsafe_method_without_site_header_is_forbidden() {
    let harness = platform_persistence::test_support::TestDatabase::create()
        .await
        .expect("a test database");
    let (app, _) = edge(&harness).await;
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    let request = from_site(
        "POST",
        RELEASES,
        &cookie,
        false,
        Some(&format!("https://{HOSTNAME}")),
        &Value::Null,
    );
    assert_eq!(
        status_and_code(&app, request).await,
        (403, "platform.auth.forbidden".to_owned())
    );
    assert!(
        outbox(harness.pool(), BUILD).await.is_empty(),
        "a command was written"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "The origin is another host", shown on the release route.
#[tokio::test]
async fn foreign_origin_is_forbidden() {
    let harness = platform_persistence::test_support::TestDatabase::create()
        .await
        .expect("a test database");
    let (app, _) = edge(&harness).await;
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    for origin in [
        Some(format!("https://{OTHER_HOSTNAME}")),
        Some(format!("http://{HOSTNAME}")),
        None,
    ] {
        let request = from_site(
            "POST",
            RELEASES,
            &cookie,
            true,
            origin.as_deref(),
            &Value::Null,
        );
        assert_eq!(
            status_and_code(&app, request).await,
            (403, "platform.auth.forbidden".to_owned()),
            "Origin {origin:?}"
        );
    }
    assert!(
        outbox(harness.pool(), BUILD).await.is_empty(),
        "a command was written"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A preflight request": no route under `/v1/sites/{site}/` answers
/// a CORS preflight.
#[tokio::test]
async fn no_cors_preflight_on_site_routes() {
    let harness = platform_persistence::test_support::TestDatabase::create()
        .await
        .expect("a test database");
    let (app, _) = edge(&harness).await;
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    for path in [
        "/v1/sites/site-a/edits",
        RELEASES,
        "/v1/sites/site-a/sessions",
        "/v1/sites/site-a/content/pages/home",
    ] {
        let request = Request::builder()
            .method("OPTIONS")
            .uri(path)
            .header("cookie", format!("{COOKIE}={cookie}"))
            .header("origin", format!("https://{HOSTNAME}"))
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .expect("a request");
        let (_, headers, _) = send(&app, request).await;
        assert!(
            headers
                .keys()
                .all(|name| !name.as_str().starts_with("access-control-")),
            "OPTIONS {path} answered a CORS header: {headers:?}"
        );
    }

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "The approval is withdrawn": the withdrawal revokes the site's owner
/// sessions in its transaction.
#[tokio::test]
async fn withdrawn_approval_revokes_site_sessions() {
    let harness = platform_persistence::test_support::TestDatabase::create()
        .await
        .expect("a test database");
    let (app, _) = edge(&harness).await;
    let cookie = sign_in(&app, SITE, HOSTNAME).await;

    let request = Request::builder()
        .method("DELETE")
        .uri("/v1/sites/site-a/manifest/approval")
        .header("authorization", format!("Bearer {OWNER}"))
        .body(Body::empty())
        .expect("a request");
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "the withdrawal: {body}");
    let live: i64 = sqlx::query_scalar(
        "select count(*) from identity.sessions where kind = 'site_owner' and revoked_at is null",
    )
    .fetch_one(harness.pool())
    .await
    .expect("a count");
    assert_eq!(
        live, 0,
        "a site_owner session of the withdrawn site is live"
    );
    let request = from_site(
        "POST",
        RELEASES,
        &cookie,
        true,
        Some(&format!("https://{HOSTNAME}")),
        &Value::Null,
    );
    assert_eq!(
        status_and_code(&app, request).await,
        (401, "platform.auth.unauthenticated".to_owned()),
        "the next request of the session"
    );

    harness.cleanup().await.expect("cleanup");
}
