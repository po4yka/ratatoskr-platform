//! Owner operation inspection through the public Edge router.

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
use ratatoskr_error_contracts::{ErrorCode, ErrorEnvelope};
use ratatoskr_identifiers::{Extensions, SafeMessage};
use ratatoskr_operation_contracts::OperationStatus;
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "admin-operations-owner-credential";
const AUDIENCE: &str = "edge";
const CAPTURE_KIND: &str = "content.capture.submit";
const SYNC_KIND: &str = "social.source.sync";
const FAILURE_CODE: &str = "content.extraction.invalid_document";
const PRIVATE_DIAGNOSTIC: &str = "postgresql://worker.internal/private-archive";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn state(harness: &TestDatabase) -> ApiState {
    let health = Arc::new(platform_http::RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    ApiState::new(harness.database.clone(), AUDIENCE, health, true)
}

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
        platform_public_api::routes(Arc::new(state)),
    )
}

async fn seed_user(pool: &sqlx::PgPool, credential: Option<&str>) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    if let Some(credential) = credential {
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
    user.user_id
}

async fn seed_operation(
    pool: &sqlx::PgPool,
    owner_user_id: Uuid,
    kind: &str,
    minutes_ago: i64,
) -> Uuid {
    let operation = platform_operations::accept(
        pool,
        owner_user_id,
        kind,
        "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39",
        None,
        now() - jiff::SignedDuration::from_mins(minutes_ago),
    )
    .await
    .expect("an operation");
    operation.operation_id
}

async fn advance(pool: &sqlx::PgPool, operation_id: Uuid, status: OperationStatus) {
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_operations::record_status(
        &mut transaction,
        operation_id,
        status,
        None,
        None,
        None,
        now(),
    )
    .await
    .expect("a legal transition");
    transaction.commit().await.expect("commit");
}

async fn fail_with_private_diagnostic(pool: &sqlx::PgPool, operation_id: Uuid) {
    let mut extensions = Extensions::new();
    extensions.insert(
        "private_diagnostic",
        serde_json::Value::String(PRIVATE_DIAGNOSTIC.to_owned()),
    );
    let error = ErrorEnvelope {
        code: ErrorCode::parse(FAILURE_CODE).expect("a stable failure code"),
        message: SafeMessage::parse("The operation failed.").expect("a safe message"),
        retryable: false,
        field_violations: Vec::new(),
        correlation_id: None,
        trace_id: None,
        extensions,
    };

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_operations::record_status(
        &mut transaction,
        operation_id,
        OperationStatus::Failed,
        Some("worker"),
        None,
        Some(PRIVATE_DIAGNOSTIC),
        now(),
    )
    .await
    .expect("a failed transition");
    platform_operations::record_error(&mut transaction, operation_id, &error, now())
        .await
        .expect("a stored failure");
    transaction.commit().await.expect("commit");
}

fn get(credential: &str, uri: impl AsRef<str>) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri.as_ref())
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .expect("a request")
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

fn item_ids(body: &serde_json::Value) -> Vec<&str> {
    body["items"]
        .as_array()
        .expect("an items array")
        .iter()
        .map(|item| item["operation_id"].as_str().expect("an operation id"))
        .collect()
}

#[tokio::test]
async fn owner_lists_and_reads_cross_user_operations_while_member_is_denied() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let actor_user_id = seed_user(pool, Some(CREDENTIAL)).await;
    let foreign_user_id = seed_user(pool, None).await;

    let oldest = seed_operation(pool, actor_user_id, CAPTURE_KIND, 30).await;
    let middle = seed_operation(pool, foreign_user_id, CAPTURE_KIND, 20).await;
    advance(pool, middle, OperationStatus::Running).await;
    let failed = seed_operation(pool, foreign_user_id, SYNC_KIND, 10).await;
    fail_with_private_diagnostic(pool, failed).await;
    let app = app(state(&harness));

    let (status, denied) = send(&app, get(CREDENTIAL, "/v1/admin/operations")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    assert!(
        denied.get("items").is_none(),
        "a refusal must contain no rows"
    );
    let denied_wire = serde_json::to_string(&denied).expect("JSON");
    for operation_id in [oldest, middle, failed] {
        assert!(!denied_wire.contains(&operation_id.to_string()), "{denied}");
    }

    platform_identity::grant::grant(pool, actor_user_id, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("the live owner grant");

    let (status, all) = send(&app, get(CREDENTIAL, "/v1/admin/operations")).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(
        item_ids(&all),
        [failed.to_string(), middle.to_string(), oldest.to_string()],
        "deployment-wide rows must be newest first: {all}"
    );

    let failed_summary = all["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["operation_id"] == failed.to_string())
        .expect("the failed summary");
    assert_eq!(failed_summary["failure_code"], FAILURE_CODE);
    assert!(failed_summary.get("message").is_none(), "{failed_summary}");
    assert!(failed_summary.get("errors").is_none(), "{failed_summary}");
    assert!(
        !serde_json::to_string(failed_summary)
            .expect("JSON")
            .contains(PRIVATE_DIAGNOSTIC),
        "{failed_summary}"
    );

    let (_, first) = send(&app, get(CREDENTIAL, "/v1/admin/operations?limit=2")).await;
    assert_eq!(item_ids(&first), [failed.to_string(), middle.to_string()]);
    let cursor = first["next_cursor"].as_str().expect("a cursor");
    let (_, second) = send(
        &app,
        get(
            CREDENTIAL,
            format!("/v1/admin/operations?limit=2&cursor={cursor}"),
        ),
    )
    .await;
    assert_eq!(item_ids(&second), [oldest.to_string()]);
    assert!(second.get("next_cursor").is_none(), "{second}");

    let (_, by_state) = send(&app, get(CREDENTIAL, "/v1/admin/operations?state=failed")).await;
    assert_eq!(item_ids(&by_state), [failed.to_string()]);
    let (_, by_kind) = send(
        &app,
        get(CREDENTIAL, format!("/v1/admin/operations?kind={SYNC_KIND}")),
    )
    .await;
    assert_eq!(item_ids(&by_kind), [failed.to_string()]);
    let (_, by_owner) = send(
        &app,
        get(
            CREDENTIAL,
            format!("/v1/admin/operations?owner_user_id={foreign_user_id}"),
        ),
    )
    .await;
    assert_eq!(
        item_ids(&by_owner),
        [failed.to_string(), middle.to_string()]
    );

    let (status, snapshot) = send(
        &app,
        get(CREDENTIAL, format!("/v1/admin/operations/{failed}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    assert_eq!(snapshot["operation_id"], failed.to_string());
    assert_eq!(snapshot["status"], "failed");

    let (ordinary_status, ordinary) =
        send(&app, get(CREDENTIAL, format!("/v1/operations/{failed}"))).await;
    assert_eq!(ordinary_status, StatusCode::NOT_FOUND, "{ordinary}");
    assert_eq!(ordinary["code"], "platform.resource.not_found");

    harness.cleanup().await.expect("cleanup");
}
