//! Owner schedule-status inspection through the public Edge router.

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
use jiff::{SignedDuration, Timestamp};
use platform_core::RuntimeRole;
use platform_core::config::PublicConfig;
use platform_http::HttpState;
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_operation_contracts::OperationStatus;
use ratatoskr_operational_contracts::{PLATFORM_OWNER_GRANT, ScheduleInspectionPage};
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "admin-schedules-owner-credential";
const AUDIENCE: &str = "edge";
const PRIVATE_PAYLOAD_VALUE: &str = "schedule-secret-account";
const PRIVATE_ENDPOINT: &str = "postgresql://scheduler.internal/private";
const PRIVATE_CRON: &str = "0 3 * * *";

fn now() -> Timestamp {
    Timestamp::now()
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

async fn seed_actor(pool: &sqlx::PgPool) -> Uuid {
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
            token: Some(auth::credential_digest(CREDENTIAL)),
            issued_at: now(),
            expires_at: now() + SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
}

struct ScheduleFixture {
    schedule_id: Uuid,
    owner_user_id: Uuid,
    service_name: &'static str,
    name: &'static str,
    next_due_at: Timestamp,
    enabled: bool,
}

impl ScheduleFixture {
    async fn insert(&self, pool: &sqlx::PgPool) {
        let created_at = self.next_due_at - SignedDuration::from_hours(24);
        sqlx::query(
            "insert into operations.schedules
                 (schedule_id, service_name, name, owner_user_id, command_type, operation_kind,
                  payload, cron_expression, next_due_at, enabled, created_at, updated_at)
             values ($1, $2, $3, $4, 'github.sync.requested.v1', 'github.sync', $5, $6,
                     $7::timestamptz, $8, $9::timestamptz, $9::timestamptz)",
        )
        .bind(self.schedule_id)
        .bind(self.service_name)
        .bind(self.name)
        .bind(self.owner_user_id)
        .bind(serde_json::json!({
            "account": PRIVATE_PAYLOAD_VALUE,
            "endpoint": PRIVATE_ENDPOINT,
        }))
        .bind(PRIVATE_CRON)
        .bind(self.next_due_at.to_string())
        .bind(self.enabled)
        .bind(created_at.to_string())
        .execute(pool)
        .await
        .expect("a fixture schedule");
    }
}

async fn attach_failed_occurrence(pool: &sqlx::PgPool, schedule: &ScheduleFixture) {
    let due_at = schedule.next_due_at - SignedDuration::from_hours(24);
    let operation = platform_operations::accept(
        pool,
        schedule.owner_user_id,
        "github.sync",
        "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39",
        None,
        due_at - SignedDuration::from_secs(1),
    )
    .await
    .expect("a scheduled operation");
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_operations::record_status(
        &mut transaction,
        operation.operation_id,
        OperationStatus::Failed,
        None,
        None,
        None,
        due_at,
    )
    .await
    .expect("a failed outcome");
    transaction.commit().await.expect("commit");

    sqlx::query(
        "insert into operations.schedule_occurrences
             (occurrence_id, schedule_id, due_at, published_at, drift_seconds, operation_id)
         values ($1, $2, $3::timestamptz, $3::timestamptz, 0, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(schedule.schedule_id)
    .bind(due_at.to_string())
    .bind(operation.operation_id)
    .execute(pool)
    .await
    .expect("a schedule occurrence");
}

fn request(uri: impl AsRef<str>) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri.as_ref())
        .header("authorization", format!("Bearer {CREDENTIAL}"))
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

#[tokio::test]
async fn owner_reads_schedule_status_without_payloads() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let actor_user_id = seed_actor(pool).await;
    let schedule_owner = platform_identity::user::create_user(pool, now())
        .await
        .expect("a schedule owner")
        .user_id;
    let first_due = Timestamp::from_second(1_900_000_000).expect("a fixture timestamp");
    let never_run = ScheduleFixture {
        schedule_id: Uuid::now_v7(),
        owner_user_id: actor_user_id,
        service_name: "github",
        name: "never-run",
        next_due_at: first_due,
        enabled: true,
    };
    let disabled_failed = ScheduleFixture {
        schedule_id: Uuid::now_v7(),
        owner_user_id: schedule_owner,
        service_name: "x",
        name: "disabled-failed",
        next_due_at: first_due + SignedDuration::from_hours(1),
        enabled: false,
    };
    never_run.insert(pool).await;
    disabled_failed.insert(pool).await;
    attach_failed_occurrence(pool, &disabled_failed).await;
    let app = app(state(&harness));

    let (status, denied) = send(&app, request("/v1/admin/schedules")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let denied_wire = serde_json::to_string(&denied).expect("JSON");
    for schedule_id in [never_run.schedule_id, disabled_failed.schedule_id] {
        assert!(!denied_wire.contains(&schedule_id.to_string()), "{denied}");
    }

    platform_identity::grant::grant(pool, actor_user_id, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("the live owner grant");

    let (status, all_body) = send(&app, request("/v1/admin/schedules")).await;
    assert_eq!(status, StatusCode::OK, "{all_body}");
    let all: ScheduleInspectionPage =
        serde_json::from_value(all_body.clone()).expect("the shared schedule page contract");
    assert_eq!(all.items.len(), 2, "{all_body}");
    assert_eq!(all.items[0].schedule_id, never_run.schedule_id);
    assert_eq!(all.items[1].schedule_id, disabled_failed.schedule_id);
    assert!(all.items[0].enabled);
    assert_eq!(all.items[0].last_outcome, None, "a never-run schedule");
    assert!(!all.items[1].enabled);
    assert_eq!(all.items[1].last_outcome, Some(OperationStatus::Failed));

    let all_wire = serde_json::to_string(&all_body).expect("JSON");
    for private in [
        PRIVATE_PAYLOAD_VALUE,
        PRIVATE_ENDPOINT,
        PRIVATE_CRON,
        "github.sync.requested.v1",
        "cron_expression",
        "command_type",
        "payload",
    ] {
        assert!(!all_wire.contains(private), "leaked {private}: {all_body}");
    }

    let (status, first_body) = send(&app, request("/v1/admin/schedules?limit=1")).await;
    assert_eq!(status, StatusCode::OK, "{first_body}");
    let first: ScheduleInspectionPage =
        serde_json::from_value(first_body.clone()).expect("the first shared page");
    assert_eq!(first.items.len(), 1, "{first_body}");
    assert_eq!(first.items[0].schedule_id, never_run.schedule_id);
    let cursor = first_body["next_cursor"].as_str().expect("a cursor");

    let (status, second_body) = send(
        &app,
        request(format!("/v1/admin/schedules?limit=1&cursor={cursor}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second_body}");
    let second: ScheduleInspectionPage =
        serde_json::from_value(second_body.clone()).expect("the second shared page");
    assert_eq!(second.items.len(), 1, "{second_body}");
    assert_eq!(second.items[0].schedule_id, disabled_failed.schedule_id);
    assert!(second.next_cursor.is_none(), "{second_body}");

    harness.cleanup().await.expect("cleanup");
}
