//! Authorization conformance matrix for every owner-only operational route.

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
use platform_core::RuntimeRole;
use platform_core::config::PublicConfig;
use platform_http::HttpState;
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_error_contracts::ErrorEnvelope;
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "admin-matrix-owner-credential";
const AUDIENCE: &str = "edge";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn app(harness: &TestDatabase) -> Router {
    let health = Arc::new(platform_http::RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    let state = ApiState::new(harness.database.clone(), AUDIENCE, health, true);
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
        platform_public_api::routes(Arc::new(state)),
    )
}

async fn seed_actor(pool: &sqlx::PgPool) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("an actor");
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind: SessionKind::Browser,
            device_id: None,
            audience: AUDIENCE,
            token: Some(auth::credential_digest(CREDENTIAL)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
}

fn request(path: &str, credential: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder.body(Body::empty()).expect("a request")
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
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn assert_refusal(
    path: &str,
    status: StatusCode,
    body: &serde_json::Value,
    expected_status: StatusCode,
    expected_code: &str,
    private_ids: &[String],
) {
    assert_eq!(status, expected_status, "GET {path}: {body}");
    let envelope: ErrorEnvelope = serde_json::from_value(body.clone())
        .unwrap_or_else(|error| panic!("GET {path} must return ErrorEnvelope: {error}: {body}"));
    assert_eq!(envelope.code.as_str(), expected_code, "GET {path}: {body}");
    assert!(
        body.get("items").is_none()
            && body.get("operations").is_none()
            && body.get("results").is_none(),
        "GET {path} refusal leaked a payload: {body}"
    );
    let wire = serde_json::to_string(body).expect("JSON");
    for private_id in private_ids {
        assert!(
            !wire.contains(private_id),
            "GET {path} refusal leaked {private_id}: {body}"
        );
    }
}

#[tokio::test]
async fn every_admin_route_rechecks_owner_and_fails_closed() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let actor_user_id = seed_actor(pool).await;
    let operation_owner = platform_identity::user::create_user(pool, now())
        .await
        .expect("an operation owner");
    let operation = platform_operations::accept(
        pool,
        operation_owner.user_id,
        "content.capture.submit",
        "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39",
        None,
        now(),
    )
    .await
    .expect("an operation");
    let operation_id = operation.operation_id.to_string();
    let actor_id = actor_user_id.to_string();
    let private_ids = [operation_id.clone(), actor_id];
    let routes = [
        "/v1/admin/operations".to_owned(),
        format!("/v1/admin/operations/{operation_id}"),
        "/v1/admin/schedules".to_owned(),
        "/v1/admin/audit-events".to_owned(),
    ];
    let app = app(&harness);

    for path in &routes {
        let (status, body) = send(&app, request(path, None)).await;
        assert_refusal(
            path,
            status,
            &body,
            StatusCode::UNAUTHORIZED,
            "platform.auth.unauthenticated",
            &private_ids,
        );
    }

    for path in &routes {
        let (status, body) = send(&app, request(path, Some(CREDENTIAL))).await;
        assert_refusal(
            path,
            status,
            &body,
            StatusCode::FORBIDDEN,
            "platform.auth.forbidden",
            &private_ids,
        );
    }

    platform_identity::grant::grant(pool, actor_user_id, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("the live owner grant");
    for path in &routes {
        let (status, body) = send(&app, request(path, Some(CREDENTIAL))).await;
        assert_eq!(status, StatusCode::OK, "owner GET {path}: {body}");
    }

    assert!(
        platform_identity::grant::revoke(pool, actor_user_id, PLATFORM_OWNER_GRANT, now(),)
            .await
            .expect("the owner revocation")
    );
    for path in &routes {
        let (status, body) = send(&app, request(path, Some(CREDENTIAL))).await;
        assert_refusal(
            path,
            status,
            &body,
            StatusCode::FORBIDDEN,
            "platform.auth.forbidden",
            &private_ids,
        );
    }

    platform_identity::grant::grant(pool, actor_user_id, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("a restored live owner grant");
    sqlx::query("drop table identity.grants")
        .execute(pool)
        .await
        .expect("the disposable grant lookup is unavailable");
    for path in &routes {
        let (status, body) = send(&app, request(path, Some(CREDENTIAL))).await;
        assert_refusal(
            path,
            status,
            &body,
            StatusCode::GATEWAY_TIMEOUT,
            "platform.request.timeout",
            &private_ids,
        );
    }

    harness.cleanup().await.expect("cleanup");
}
