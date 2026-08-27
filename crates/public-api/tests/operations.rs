//! The cancellation and listing routes, end to end against the real pipeline.

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
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "operations-route-credential";
const SECOND_CREDENTIAL: &str = "operations-second-credential";
const AUDIENCE: &str = "edge";
const KIND: &str = "content.capture.submit";
const OTHER_KIND: &str = "social.source.sync";
const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";

/// The subject every accepted cancellation must enqueue exactly once.
const CANCEL_SUBJECT: &str = "cmd.platform.operation.cancel_requested.v1";

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
        platform_public_api::routes(std::sync::Arc::new(state)),
    )
}

async fn seed(pool: &sqlx::PgPool, credential: &str, audience: &str) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind: SessionKind::Browser,
            device_id: None,
            audience,
            token: Some(auth::credential_digest(credential)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
}

/// One owned operation in the given state, with its own acceptance instant.
async fn seed_operation(pool: &sqlx::PgPool, owner: Uuid, kind: &str, minutes_ago: i64) -> Uuid {
    let accepted_at = now() - jiff::SignedDuration::from_secs(60 * minutes_ago);
    let operation = platform_operations::accept(pool, owner, kind, CORRELATION, None, accepted_at)
        .await
        .expect("an operation");
    operation.operation_id
}

async fn advance(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    status: ratatoskr_operation_contracts::OperationStatus,
) {
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

fn cancel(credential: Option<&str>, operation_id: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/v1/operations/{operation_id}/cancel"));
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder.body(Body::empty()).expect("a request")
}

fn list(credential: Option<&str>, query: &str) -> Request<Body> {
    let mut builder = Request::builder().uri(format!("/v1/operations{query}"));
    if let Some(credential) = credential {
        builder = builder.header("authorization", format!("Bearer {credential}"));
    }
    builder.body(Body::empty()).expect("a request")
}

fn read_one(credential: Option<&str>, operation_id: &str) -> Request<Body> {
    let mut builder = Request::builder().uri(format!("/v1/operations/{operation_id}"));
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
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// How many commands of each subject the outbox holds for one operation.
async fn outbox_subjects(pool: &sqlx::PgPool, operation_id: Uuid) -> Vec<(String, i64)> {
    sqlx::query_as(
        "select subject, count(*) from operations.outbox
          where operation_id = $1 group by subject order by subject",
    )
    .bind(operation_id)
    .fetch_all(pool)
    .await
    .expect("reading the outbox")
}

/// R-1. Cancelling a pending operation accepts the request, leaves the status truthful, and
/// enqueues exactly one cancellation command — a replay adds none.
#[tokio::test]
async fn a_session_cancels_its_own_pending_operation_once() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));
    let operation_id = seed_operation(pool, user, KIND, 5).await;
    let id = operation_id.to_string();

    let (status, body) = send(&app, cancel(Some(CREDENTIAL), &id)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["operation_id"], body["operation_id"]);
    assert_eq!(
        body["status"], "accepted",
        "the acceptance carries current truth, not the requested outcome"
    );
    assert_eq!(body["kind"], KIND);

    let subjects = outbox_subjects(pool, operation_id).await;
    assert_eq!(
        subjects,
        vec![(CANCEL_SUBJECT.to_owned(), 1)],
        "exactly one cancellation command"
    );

    // The command names its target and tenant.
    let payload: serde_json::Value = sqlx::query_scalar(
        "select payload from operations.outbox where operation_id = $1 and subject = $2",
    )
    .bind(operation_id)
    .bind(CANCEL_SUBJECT)
    .fetch_one(pool)
    .await
    .expect("the command payload");
    assert_eq!(payload["operation_id"], json_id(&operation_id.to_string()));
    assert_eq!(
        payload["tenant_id"],
        format!("user:{user}"),
        "the owner rides as tenant context"
    );
    assert_eq!(payload["correlation_id"], CORRELATION);

    // A replay is acceptance again, and queues nothing further.
    let (replay_status, replay_body) = send(&app, cancel(Some(CREDENTIAL), &id)).await;
    assert_eq!(replay_status, StatusCode::ACCEPTED);
    assert_eq!(replay_body["status"], "accepted");
    assert_eq!(
        outbox_subjects(pool, operation_id).await,
        vec![(CANCEL_SUBJECT.to_owned(), 1)],
        "the repeat must not enqueue a second command"
    );

    harness.cleanup().await.expect("cleanup");
}

/// JSON string form of a UUID, for comparing into `serde_json::Value` payloads.
fn json_id(id: &str) -> serde_json::Value {
    serde_json::Value::String(id.to_owned())
}

/// R-2. A finished operation answers with what already happened; nothing is written.
#[tokio::test]
async fn terminal_operations_answer_with_current_truth() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let succeeded = seed_operation(pool, user, KIND, 5).await;
    advance(
        pool,
        succeeded,
        ratatoskr_operation_contracts::OperationStatus::Succeeded,
    )
    .await;

    let cancelled = seed_operation(pool, user, KIND, 4).await;
    advance(
        pool,
        cancelled,
        ratatoskr_operation_contracts::OperationStatus::Cancelled,
    )
    .await;

    let (status, body) = send(&app, cancel(Some(CREDENTIAL), &succeeded.to_string())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "succeeded");
    assert_eq!(
        outbox_subjects(pool, succeeded).await,
        vec![],
        "truth needs no command"
    );

    let (status, body) = send(&app, cancel(Some(CREDENTIAL), &cancelled.to_string())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["status"], "cancelled",
        "cancelling an already-cancelled operation is plain truth, not a conflict"
    );
    assert_eq!(outbox_subjects(pool, cancelled).await, vec![]);

    harness.cleanup().await.expect("cleanup");
}

/// R-3. Another user's operation is indistinguishable from a missing one.
#[tokio::test]
async fn another_users_operation_is_not_found() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed(pool, CREDENTIAL, AUDIENCE).await;
    seed(pool, SECOND_CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));
    let foreign = seed_operation(pool, owner, KIND, 5).await;

    let (status, body) = send(&app, cancel(Some(SECOND_CREDENTIAL), &foreign.to_string())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "platform.resource.not_found");

    let (missing, missing_body) = send(
        &app,
        cancel(Some(SECOND_CREDENTIAL), &Uuid::now_v7().to_string()),
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
    assert_eq!(
        missing_body["code"], "platform.resource.not_found",
        "a nonexistent identifier must look exactly like somebody else's"
    );

    let marked: i64 = sqlx::query_scalar(
        "select count(*) from operations.operations where cancellation_requested_at is not null",
    )
    .fetch_one(pool)
    .await
    .expect("counting markers");
    assert_eq!(marked, 0, "a refused caller records nothing");

    harness.cleanup().await.expect("cleanup");
}

/// R-4. No credential, no cancellation.
#[tokio::test]
async fn unauthenticated_cancellation_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    seed(pool, CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));
    let operation_id = seed_operation(pool, Uuid::now_v7(), KIND, 5).await;

    let (status, body) = send(&app, cancel(None, &operation_id.to_string())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "platform.auth.unauthenticated");

    harness.cleanup().await.expect("cleanup");
}

/// R-5. A path segment that is not a UUID stays a client error with an envelope.
#[tokio::test]
async fn a_malformed_operation_identifier_is_a_client_error() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let (status, body) = send(&app, cancel(Some(CREDENTIAL), "not-a-uuid")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "platform.request.invalid");

    harness.cleanup().await.expect("cleanup");
}

/// A result reference recorded against a fixture operation, so the singular endpoint has a heavy
/// payload to carry and the listing must not.
async fn attach_result(pool: &sqlx::PgPool, operation_id: Uuid) {
    use ratatoskr_identifiers::{EntityRef, Extensions};
    use ratatoskr_operation_contracts::{OperationResultKind, OperationResultRef};

    platform_operations::record_result(
        pool,
        operation_id,
        &OperationResultRef {
            result_kind: OperationResultKind::parse("content.document").expect("a result kind"),
            target: EntityRef::parse("document:01a0153f-63e5-7010-a4c9-1fe6c43bcc40")
                .expect("a target"),
            blob: None,
            ai_archive_import_summary: None,
            extensions: Extensions::default(),
        },
        now(),
    )
    .await
    .expect("recording a result");
}

/// R-6. The listing is scoped to the caller, filtered explicitly, paginated over a stable cursor,
/// and carries summaries without heavy payloads.
#[tokio::test]
async fn listing_is_scoped_filtered_and_paginated() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = seed(pool, CREDENTIAL, AUDIENCE).await;
    let stranger = seed(pool, SECOND_CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    // Owner's fixture, oldest first; l2 runs, l3 succeeds in another kind and carries a result.
    let l1 = seed_operation(pool, owner, KIND, 30).await;
    let l2 = seed_operation(pool, owner, KIND, 20).await;
    advance(
        pool,
        l2,
        ratatoskr_operation_contracts::OperationStatus::Running,
    )
    .await;
    let l3 = seed_operation(pool, owner, OTHER_KIND, 10).await;
    advance(
        pool,
        l3,
        ratatoskr_operation_contracts::OperationStatus::Succeeded,
    )
    .await;
    attach_result(pool, l3).await;

    // The stranger's row must never appear in the owner's pages.
    let strangers = seed_operation(pool, stranger, KIND, 25).await;

    // Unfiltered: newest first, tenant-scoped.
    let (status, body) = send(&app, list(Some(CREDENTIAL), "")).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["operations"]
        .as_array()
        .expect("an operations array")
        .iter()
        .map(|row| row["operation_id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids.len(), 3, "{body}");
    assert!(!ids.contains(&strangers.to_string().as_str()));
    assert_eq!(ids[0], l3.to_string(), "newest accepted first");
    assert_eq!(body["next_cursor"], serde_json::Value::Null);

    // Single filters and their conjunction.
    let (_, body) = send(&app, list(Some(CREDENTIAL), "?state=running")).await;
    assert_eq!(body["operations"].as_array().expect("rows").len(), 1);
    assert_eq!(body["operations"][0]["operation_id"], l2.to_string());
    assert_eq!(body["operations"][0]["status"], "running");

    let (_, body) = send(&app, list(Some(CREDENTIAL), &format!("?kind={OTHER_KIND}"))).await;
    assert_eq!(body["operations"][0]["operation_id"], l3.to_string());

    let (_, body) = send(
        &app,
        list(
            Some(CREDENTIAL),
            &format!("?state=running&kind={OTHER_KIND}"),
        ),
    )
    .await;
    assert_eq!(
        body["operations"].as_array().expect("rows").len(),
        0,
        "conjunction filters"
    );

    // Invalid filter values are client errors, never silent empty pages.
    for bad in [
        "?state=bogus",
        "?kind=BROKEN",
        "?limit=0",
        "?limit=101",
        "?cursor=garbage",
    ] {
        let (status, body) = send(&app, list(Some(CREDENTIAL), bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["code"], "platform.request.invalid", "{bad}");
    }

    // Pagination: two per page, then the remainder; no next page after the last row.
    let (_, first) = send(&app, list(Some(CREDENTIAL), "?limit=2")).await;
    let rows = first["operations"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a continuation cursor");
    assert!(!cursor.is_empty());

    let (_, second) = send(&app, list(Some(CREDENTIAL), &format!("?cursor={cursor}"))).await;
    let rest = second["operations"].as_array().expect("rows");
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0]["operation_id"], l1.to_string());
    assert_eq!(second["next_cursor"], serde_json::Value::Null);

    // Summaries carry lifecycle fields but not payloads; the singular endpoint still does.
    for row in first["operations"].as_array().expect("rows") {
        assert!(row.get("results").is_none(), "{}", row);
        assert!(row.get("errors").is_none(), "{}", row);
        assert!(row.get("warnings").is_none(), "{}", row);
        assert!(row["operation_id"].is_string());
        assert!(row["status"].is_string());
        assert!(row["kind"].is_string());
        assert!(row["accepted_at"].is_string());
        assert!(row["correlation_id"].is_string());
    }
    let (_, snapshot) = send(&app, read_one(Some(CREDENTIAL), &l3.to_string())).await;
    assert!(
        snapshot["results"].as_array().is_some_and(|r| r.len() == 1),
        "the singular endpoint keeps its payload: {snapshot}"
    );

    harness.cleanup().await.expect("cleanup");
}

/// R-7. No credential, no listing.
#[tokio::test]
async fn unauthenticated_listing_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    seed(harness.pool(), CREDENTIAL, AUDIENCE).await;
    let app = app(state(&harness));

    let (status, body) = send(&app, list(None, "")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "platform.auth.unauthenticated");

    harness.cleanup().await.expect("cleanup");
}
