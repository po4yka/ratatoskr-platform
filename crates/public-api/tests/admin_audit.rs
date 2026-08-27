//! Owner audit inspection through the public Edge router.

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
use platform_identity::audit::{AuditEvent, AuditOutcome as StoredAuditOutcome};
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use ratatoskr_operational_contracts::{AuditEventPage, AuditOutcome, PLATFORM_OWNER_GRANT};
use tower::ServiceExt as _;
use uuid::Uuid;

const CREDENTIAL: &str = "admin-audit-owner-credential-private";
const AUDIENCE: &str = "edge";
const PRIVATE_URL: &str = "https://private.internal/archive?token=fixture-secret";
const PRIVATE_BODY: &str = "private request body and diagnostic";
const SYSTEM_CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc41";
const USER_CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc42";
const OLDER_CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc43";

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

async fn seed_actor(pool: &sqlx::PgPool) -> (Uuid, Uuid) {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let session = platform_identity::session::create_session(
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
    (user.user_id, session.session_id)
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
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end authorization and pagination scenario keeps its ordered evidence together"
)]
async fn owner_reads_stable_redacted_audit_pages() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (actor_user_id, actor_session_id) = seed_actor(pool).await;
    let target_id = Uuid::now_v7();
    let same_occurred_at = jiff::Timestamp::from_second(1_900_000_000).expect("a timestamp");
    let lower_tie_id =
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("a fixture UUID");
    let higher_tie_id =
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").expect("a fixture UUID");
    let older_id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").expect("a fixture UUID");

    platform_identity::audit::record(
        pool,
        &AuditEvent {
            audit_event_id: lower_tie_id,
            actor_user_id: Some(actor_user_id),
            actor_session_id: Some(actor_session_id),
            action: "credential.revoke",
            target_kind: "credential",
            target_id: Some(target_id),
            outcome: StoredAuditOutcome::Allowed,
            correlation_id: USER_CORRELATION.to_owned(),
        },
        same_occurred_at,
    )
    .await
    .expect("the user audit event");
    platform_identity::audit::record(
        pool,
        &AuditEvent {
            audit_event_id: higher_tie_id,
            actor_user_id: None,
            actor_session_id: None,
            action: "system.health_observed",
            target_kind: "system",
            target_id: None,
            outcome: StoredAuditOutcome::Failed,
            correlation_id: SYSTEM_CORRELATION.to_owned(),
        },
        same_occurred_at,
    )
    .await
    .expect("the system audit event");
    platform_identity::audit::record(
        pool,
        &AuditEvent {
            audit_event_id: older_id,
            actor_user_id: Some(actor_user_id),
            actor_session_id: Some(actor_session_id),
            action: "session.create",
            target_kind: "session",
            target_id: Some(actor_session_id),
            outcome: StoredAuditOutcome::Denied,
            correlation_id: OLDER_CORRELATION.to_owned(),
        },
        same_occurred_at - jiff::SignedDuration::from_secs(1),
    )
    .await
    .expect("the older audit event");
    let app = app(state(&harness));

    let (status, denied) = send(&app, request("/v1/admin/audit-events")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let denied_wire = serde_json::to_string(&denied).expect("JSON");
    for audit_event_id in [higher_tie_id, lower_tie_id, older_id] {
        assert!(
            !denied_wire.contains(&audit_event_id.to_string()),
            "{denied}"
        );
    }

    platform_identity::grant::grant(pool, actor_user_id, PLATFORM_OWNER_GRANT, now(), None)
        .await
        .expect("the live owner grant");

    let (status, all_body) = send(&app, request("/v1/admin/audit-events")).await;
    assert_eq!(status, StatusCode::OK, "{all_body}");
    let all: AuditEventPage =
        serde_json::from_value(all_body.clone()).expect("the shared audit page contract");
    let ids: Vec<Uuid> = all.items.iter().map(|item| item.audit_event_id).collect();
    assert_eq!(
        ids,
        [higher_tie_id, lower_tie_id, older_id],
        "newest first, then audit_event_id descending: {all_body}"
    );

    let system = &all.items[0];
    assert!(system.actor_user_id.is_none());
    assert!(system.actor_session_id.is_none());
    assert_eq!(system.outcome, AuditOutcome::Failed);
    assert_eq!(all_body["items"][0]["action"], "system.health_observed");
    assert_eq!(all_body["items"][0]["target_kind"], "system");
    assert_eq!(all_body["items"][0]["target_id"], serde_json::Value::Null);
    assert_eq!(all_body["items"][0]["correlation_id"], SYSTEM_CORRELATION);

    let user = &all.items[1];
    assert!(user.actor_user_id.is_some());
    assert_eq!(user.actor_session_id, Some(actor_session_id));
    assert_eq!(user.outcome, AuditOutcome::Allowed);
    assert_eq!(all_body["items"][1]["action"], "credential.revoke");
    assert_eq!(all_body["items"][1]["target_kind"], "credential");
    assert_eq!(all_body["items"][1]["target_id"], target_id.to_string());
    assert_eq!(all_body["items"][1]["correlation_id"], USER_CORRELATION);

    let all_wire = serde_json::to_string(&all_body).expect("JSON");
    for private in [CREDENTIAL, PRIVATE_URL, PRIVATE_BODY] {
        assert!(!all_wire.contains(private), "leaked {private}: {all_body}");
    }
    for forbidden_field in ["body", "payload", "token", "diagnostic"] {
        assert!(
            !all_body["items"]
                .as_array()
                .expect("items")
                .iter()
                .any(|item| item.get(forbidden_field).is_some()),
            "leaked {forbidden_field}: {all_body}"
        );
    }

    let (status, first_body) = send(&app, request("/v1/admin/audit-events?limit=2")).await;
    assert_eq!(status, StatusCode::OK, "{first_body}");
    let first: AuditEventPage =
        serde_json::from_value(first_body.clone()).expect("the first shared page");
    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.audit_event_id)
            .collect::<Vec<_>>(),
        [higher_tie_id, lower_tie_id]
    );
    let cursor = first_body["next_cursor"].as_str().expect("a cursor");

    let (status, second_body) = send(
        &app,
        request(format!("/v1/admin/audit-events?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second_body}");
    let second: AuditEventPage =
        serde_json::from_value(second_body.clone()).expect("the second shared page");
    assert_eq!(second.items.len(), 1, "{second_body}");
    assert_eq!(second.items[0].audit_event_id, older_id);
    assert!(second.next_cursor.is_none(), "{second_body}");

    harness.cleanup().await.expect("cleanup");
}
