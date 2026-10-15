//! `POST /v1/devices/pairing-codes` for a `site_deployer` (workspace change
//! `add-site-gateway-hosting`, spec `site-bundles`, "A deployer device is paired by the owner for
//! one site"), end to end against real `PostgreSQL`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod common_session;

use axum::body::Body;
use http::{Request, StatusCode};
use platform_identity::SessionKind;
use platform_persistence::test_support::TestDatabase;
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;

use common_session::{AUDIENCE, CREDENTIAL as OWNER, app, now, seed_session, send, state};

const MEMBER: &str = "site-deployer-codes-member-000";

fn post(uri: &str, credential: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

async fn pairing_codes(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from identity.pairing_codes")
        .fetch_one(pool)
        .await
        .expect("a count")
}

/// A code for a `site_deployer` names exactly one valid site, only that kind takes a site, and only
/// a primary session with the live owner grant creates one. The code pairs a `site_deployer` for
/// that site.
#[tokio::test]
async fn a_site_deployer_code_needs_the_owner_and_one_site() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed_session(pool, OWNER, AUDIENCE, SessionKind::Browser).await;
    platform_identity::grant::grant(pool, owner, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("the owner grant");
    seed_session(pool, MEMBER, AUDIENCE, SessionKind::Browser).await;
    let app = app(state(&harness));
    let codes = "/v1/devices/pairing-codes";

    for (label, body) in [
        (
            "site_deployer without a site",
            r#"{"expected_kind":"site_deployer"}"#,
        ),
        (
            "site_deployer with a site that is not a site id",
            r#"{"expected_kind":"site_deployer","site_id":"Not A Site"}"#,
        ),
        (
            "a mobile code with a site",
            r#"{"expected_kind":"mobile","site_id":"site-a"}"#,
        ),
        ("a code of any kind with a site", r#"{"site_id":"site-a"}"#),
    ] {
        let (status, body) = send(&app, post(codes, Some(OWNER), body)).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("platform.request.invalid")),
            "{label}"
        );
    }
    let deployer = r#"{"expected_kind":"site_deployer","site_id":"site-a"}"#;
    let (status, body) = send(&app, post(codes, Some(MEMBER), deployer)).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("platform.auth.forbidden")),
        "a session without the owner grant"
    );
    assert_eq!(pairing_codes(pool).await, 0, "no refused code is stored");

    let (status, issued) = send(&app, post(codes, Some(OWNER), deployer)).await;
    assert_eq!(status, StatusCode::CREATED, "the owner's code: {issued}");
    let code = issued["code"].as_str().expect("a code");
    let pair = format!(r#"{{"code":"{code}","kind":"site_deployer"}}"#);
    let (status, paired) = send(&app, post("/v1/devices/pair", None, &pair)).await;
    assert_eq!(status, StatusCode::CREATED, "the code pairs: {paired}");
    let device_id: uuid::Uuid = paired["device_id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .expect("a device id");
    let device = platform_identity::device::find_device(pool, device_id)
        .await
        .expect("reading back")
        .expect("the device exists");
    assert_eq!(device.kind, platform_identity::DeviceKind::SiteDeployer);
    assert_eq!(device.site_id.as_deref(), Some("site-a"));

    harness.cleanup().await.expect("cleanup");
}
