//! Schedule registration integration tests.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use jiff::Timestamp;
use platform_eventing::{Incoming, MessageClass, Subject, consumer::deliver, inbox::Outcome};
use platform_identity::user::create_user;
use platform_persistence::test_support::TestDatabase;
use platform_scheduling::{RegistrationHandler, ScheduleRegistration, occurrence_id, run_once};
use ratatoskr_operation_contracts::OperationStatus;
use sqlx::Row as _;
use uuid::Uuid;

fn registration(cron: &str, producer: &str) -> Incoming {
    Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::new(
            MessageClass::Command,
            "platform.schedule.registration_requested.v1",
        )
        .expect("the registration subject is valid"),
        producer: producer.to_owned(),
        payload: serde_json::json!({
            "correlation_id": "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39",
            "payload": {
                "service_name": "ratatoskr-github",
                "name": "nightly-stars",
                "owner_user_id": Uuid::now_v7(),
                "cron_expression": cron,
                "command_type": "github.sync.requested.v1",
                "operation_kind": "github.sync",
                "payload": { "account": "po4yka" },
                "enabled": true
            }
        }),
    }
}

#[tokio::test]
async fn invalid_cron_registration_is_rejected() {
    let database = TestDatabase::create().await.expect("a test database");
    let handler = RegistrationHandler::new(vec!["ratatoskr-github".to_owned()]);
    let outcome = handler
        .register(
            database.pool(),
            &registration("bad cron", "ratatoskr-github"),
            Timestamp::now(),
        )
        .await
        .expect("invalid input is a rejected outcome, not an infrastructure error");

    assert_eq!(outcome, ScheduleRegistration::Rejected);
    let count: i64 = sqlx::query_scalar("select count(*) from operations.schedules")
        .fetch_one(database.pool())
        .await
        .expect("the count must run");
    assert_eq!(count, 0);
    database.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn unauthorized_producer_registration_is_rejected() {
    let database = TestDatabase::create().await.expect("a test database");
    let outcome = RegistrationHandler::new(vec!["ratatoskr-github".to_owned()])
        .register(
            database.pool(),
            &registration("0 3 * * *", "ratatoskr-x"),
            Timestamp::now(),
        )
        .await
        .expect("a refusal is recorded");
    assert_eq!(outcome, ScheduleRegistration::Rejected);
    database.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn malformed_registration_fields_are_rejected_before_persistence() {
    let database = TestDatabase::create().await.expect("a test database");
    let now = Timestamp::now();
    let owner = create_user(database.pool(), now).await.expect("an owner");
    let mut message = registration("0 3 * * *", "ratatoskr-github");
    message.payload["payload"]["owner_user_id"] = serde_json::json!(owner.user_id);
    message.payload["payload"]["name"] = serde_json::json!("nightly stars");
    let outcome = RegistrationHandler::new(vec!["ratatoskr-github".to_owned()])
        .register(database.pool(), &message, now)
        .await;

    assert_eq!(
        outcome.expect("a malformed registration is a rejected outcome"),
        ScheduleRegistration::Rejected
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from operations.schedules")
            .fetch_one(database.pool())
            .await
            .expect("the count must run"),
        0
    );
    database.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn redelivery_updates_one_service_named_schedule() {
    let database = TestDatabase::create().await.expect("a test database");
    let now = Timestamp::now();
    let owner = create_user(database.pool(), now).await.expect("an owner");
    let mut message = registration("0 3 * * *", "ratatoskr-github");
    message.payload["payload"]["owner_user_id"] = serde_json::json!(owner.user_id);
    let handler = RegistrationHandler::new(vec!["ratatoskr-github".to_owned()]);
    assert_eq!(
        deliver(database.pool(), &handler, &message, now)
            .await
            .expect("first delivery"),
        Some(Outcome::Applied)
    );
    let first: Uuid = sqlx::query_scalar("select schedule_id from operations.schedules")
        .fetch_one(database.pool())
        .await
        .expect("stored schedule");
    assert_eq!(
        deliver(database.pool(), &handler, &message, now)
            .await
            .expect("redelivery"),
        None
    );
    let row: (Uuid, i64) = sqlx::query_as(
        "select schedule_id, (select count(*) from operations.schedules) from operations.schedules",
    )
    .fetch_one(database.pool())
    .await
    .expect("one schedule");
    assert_eq!(row, (first, 1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*) from identity.audit_events where action = 'schedule.registration' and outcome = 'allowed'",
        )
        .fetch_one(database.pool())
        .await
        .expect("registration audit count"),
        1,
        "the inbox redelivery must not create a second audit decision",
    );
    database.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn update_and_disable_keep_the_schedule_identity() {
    let database = TestDatabase::create().await.expect("a test database");
    let now = Timestamp::from_second(1_700_000_000).expect("a timestamp");
    let owner = create_user(database.pool(), now).await.expect("an owner");
    let mut message = registration("* * * * *", "ratatoskr-github");
    message.payload["payload"]["owner_user_id"] = serde_json::json!(owner.user_id);
    let handler = RegistrationHandler::new(vec!["ratatoskr-github".to_owned()]);
    assert_eq!(
        handler
            .register(database.pool(), &message, now)
            .await
            .expect("initial registration"),
        ScheduleRegistration::Applied
    );
    let schedule_id: Uuid = sqlx::query_scalar("select schedule_id from operations.schedules")
        .fetch_one(database.pool())
        .await
        .expect("stored schedule");

    message.message_id = Uuid::now_v7();
    message.payload["payload"]["cron_expression"] = serde_json::json!("0 * * * *");
    message.payload["payload"]["payload"] = serde_json::json!({"account": "updated"});
    message.payload["payload"]["enabled"] = serde_json::json!(false);
    assert_eq!(
        handler
            .register(database.pool(), &message, now)
            .await
            .expect("disable update"),
        ScheduleRegistration::Applied
    );
    let row = sqlx::query(
        "select schedule_id, cron_expression, payload, enabled from operations.schedules",
    )
    .fetch_one(database.pool())
    .await
    .expect("updated schedule");
    assert_eq!(row.get::<Uuid, _>("schedule_id"), schedule_id);
    assert_eq!(row.get::<String, _>("cron_expression"), "0 * * * *");
    assert_eq!(
        row.get::<serde_json::Value, _>("payload")["account"],
        "updated"
    );
    assert!(!row.get::<bool, _>("enabled"));
    let report = run_once(
        database.pool(),
        32,
        now + jiff::SignedDuration::from_secs(2 * 24 * 60 * 60),
    )
    .await
    .expect("disabled schedules are skipped");
    assert_eq!(report.due, 0);
    database.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn due_occurrence_survives_a_schedule_edit_once() {
    let database = TestDatabase::create().await.expect("a test database");
    let registered_at = Timestamp::from_second(1_700_000_000).expect("a timestamp");
    let owner = create_user(database.pool(), registered_at)
        .await
        .expect("an owner");
    let mut message = registration("* * * * *", "ratatoskr-github");
    message.payload["payload"]["owner_user_id"] = serde_json::json!(owner.user_id);
    let handler = RegistrationHandler::new(vec!["ratatoskr-github".to_owned()]);
    handler
        .register(database.pool(), &message, registered_at)
        .await
        .expect("initial registration");
    let schedule_id: Uuid = sqlx::query_scalar("select schedule_id from operations.schedules")
        .fetch_one(database.pool())
        .await
        .expect("stored schedule");
    let due: time::OffsetDateTime =
        sqlx::query_scalar("select next_due_at from operations.schedules")
            .fetch_one(database.pool())
            .await
            .expect("first due time");
    let edit_at = Timestamp::from_nanosecond(due.unix_timestamp_nanos()).expect("a timestamp")
        + jiff::SignedDuration::from_secs(1);

    message.message_id = Uuid::now_v7();
    message.payload["payload"]["cron_expression"] = serde_json::json!("0 * * * *");
    handler
        .register(database.pool(), &message, edit_at)
        .await
        .expect("edit registration");
    let retained: time::OffsetDateTime =
        sqlx::query_scalar("select next_due_at from operations.schedules")
            .fetch_one(database.pool())
            .await
            .expect("retained due time");
    assert_eq!(
        retained, due,
        "an edit does not discard its selected occurrence"
    );

    let report = run_once(database.pool(), 32, edit_at)
        .await
        .expect("scheduler pass");
    assert_eq!(report.published, 1);
    let due_timestamp = Timestamp::from_nanosecond(due.unix_timestamp_nanos()).expect("due");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*) from operations.schedule_occurrences where occurrence_id = $1",
        )
        .bind(occurrence_id(schedule_id, due_timestamp))
        .fetch_one(database.pool())
        .await
        .expect("occurrence count"),
        1
    );
    assert_eq!(
        run_once(database.pool(), 32, edit_at)
            .await
            .expect("second scheduler pass")
            .due,
        0
    );
    database.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn schedule_status_reports_owner_next_due_and_last_outcome() {
    let database = TestDatabase::create().await.expect("a test database");
    let now = Timestamp::from_second(1_700_000_000).expect("a timestamp");
    let owner = create_user(database.pool(), now).await.expect("an owner");
    let mut message = registration("* * * * *", "ratatoskr-github");
    message.payload["payload"]["owner_user_id"] = serde_json::json!(owner.user_id);
    let handler = RegistrationHandler::new(vec!["ratatoskr-github".to_owned()]);
    handler
        .register(database.pool(), &message, now)
        .await
        .expect("registration");
    let due: time::OffsetDateTime =
        sqlx::query_scalar("select next_due_at from operations.schedules")
            .fetch_one(database.pool())
            .await
            .expect("first due time");
    let due_timestamp = Timestamp::from_nanosecond(due.unix_timestamp_nanos()).expect("due");
    run_once(database.pool(), 32, due_timestamp)
        .await
        .expect("scheduler pass");
    let operation_id: Uuid =
        sqlx::query_scalar("select operation_id from operations.schedule_occurrences limit 1")
            .fetch_one(database.pool())
            .await
            .expect("scheduled operation");
    let mut transaction = database.pool().begin().await.expect("transaction");
    platform_operations::record_status(
        &mut transaction,
        operation_id,
        OperationStatus::Succeeded,
        None,
        None,
        None,
        due_timestamp,
    )
    .await
    .expect("terminal outcome");
    transaction.commit().await.expect("commit outcome");

    let row = sqlx::query(
        "select service_name, owner_user_id, next_due_at, last_outcome
           from operations.schedule_status",
    )
    .fetch_one(database.pool())
    .await
    .expect("status projection");
    assert_eq!(row.get::<String, _>("service_name"), "ratatoskr-github");
    assert_eq!(row.get::<Uuid, _>("owner_user_id"), owner.user_id);
    assert!(row.get::<time::OffsetDateTime, _>("next_due_at") > due);
    assert_eq!(row.get::<String, _>("last_outcome"), "succeeded");
    database.cleanup().await.expect("cleanup");
}
