//! Publication against a real database — tests S-1 … S-8.
//!
//! Every one of these needs `PostgreSQL`, because every claim worth making here is about a
//! constraint, a lock or an `on conflict`: none of them is observable from a unit test, and the
//! deterministic-identifier machinery of `ARCHITECTURE.md` S14 is only real if the database refuses
//! the second copy.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use jiff::{SignedDuration, Timestamp};
use platform_persistence::test_support::TestDatabase;
use platform_scheduling::{occurrence_id, run_once};
use sqlx::{PgPool, Row as _};
use uuid::Uuid;

/// A registered schedule, with everything but the parts a test varies.
struct Fixture {
    schedule_id: Uuid,
    owner_user_id: Uuid,
    name: String,
    next_due_at: Timestamp,
    enabled: bool,
}

impl Fixture {
    fn new(next_due_at: Timestamp) -> Self {
        let schedule_id = Uuid::now_v7();
        Self {
            schedule_id,
            owner_user_id: Uuid::now_v7(),
            name: format!("s{}", schedule_id.simple())
                .chars()
                .take(60)
                .collect(),
            next_due_at,
            enabled: true,
        }
    }

    async fn insert(&self, pool: &PgPool) {
        let created = to_offset(self.next_due_at - SignedDuration::from_hours(1));
        sqlx::query(
            "insert into operations.schedules
                 (schedule_id, service_name, name, owner_user_id, command_type, operation_kind, payload,
                  cron_expression, next_due_at, enabled, created_at, updated_at)
             values ($1, 'ratatoskr-github', $2, $3, 'github.sync.requested.v1', 'github.sync',
                     '{\"account\": \"po4yka\"}'::jsonb, '* * * * *', $4, $5, $6, $6)",
        )
        .bind(self.schedule_id)
        .bind(&self.name)
        .bind(self.owner_user_id)
        .bind(to_offset(self.next_due_at))
        .bind(self.enabled)
        .bind(created)
        .execute(pool)
        .await
        .expect("the fixture schedule must insert");
    }
}

fn to_offset(value: Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .expect("a timestamp inside the supported range")
}

fn from_offset(value: time::OffsetDateTime) -> Timestamp {
    Timestamp::from_nanosecond(value.unix_timestamp_nanos())
        .expect("a timestamp inside the supported range")
}

async fn next_due_at(pool: &PgPool, schedule_id: Uuid) -> Timestamp {
    let stored: time::OffsetDateTime =
        sqlx::query_scalar("select next_due_at from operations.schedules where schedule_id = $1")
            .bind(schedule_id)
            .fetch_one(pool)
            .await
            .expect("the schedule must still exist");
    from_offset(stored)
}

async fn occurrence_count(pool: &PgPool, schedule_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "select count(*) from operations.schedule_occurrences where schedule_id = $1",
    )
    .bind(schedule_id)
    .fetch_one(pool)
    .await
    .expect("the count must run")
}

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_second(seconds - seconds.rem_euclid(60))
        .expect("a minute-aligned timestamp inside the supported range")
}

/// S-1. One due schedule produces exactly one occurrence, one operation and one outbox command,
/// and all three are joined by the deterministic occurrence identifier.
#[tokio::test]
async fn a_due_schedule_publishes_an_occurrence_an_operation_and_a_command() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let fixture = Fixture::new(due);
    fixture.insert(pool).await;

    let now = due + SignedDuration::from_secs(7);
    let report = run_once(pool, 32, now).await.expect("the pass must run");
    assert_eq!((report.due, report.published, report.suppressed), (1, 1, 0));

    let expected = occurrence_id(fixture.schedule_id, due);
    let row = sqlx::query(
        "select o.due_at, o.drift_seconds, o.operation_id,
                op.kind, op.owner_user_id, op.idempotency_key, op.status,
                b.subject, b.payload, b.message_id
           from operations.schedule_occurrences o
           join operations.operations op on op.operation_id = o.operation_id
           join operations.outbox b on b.operation_id = o.operation_id
          where o.occurrence_id = $1",
    )
    .bind(expected)
    .fetch_one(pool)
    .await
    .expect("the occurrence, its operation and its command must all exist");

    assert_eq!(
        from_offset(row.get::<time::OffsetDateTime, _>("due_at")),
        due
    );
    assert_eq!(row.get::<i64, _>("drift_seconds"), 7);
    assert_eq!(row.get::<String, _>("kind"), "github.sync");
    assert_eq!(row.get::<Uuid, _>("owner_user_id"), fixture.owner_user_id);
    assert_eq!(row.get::<String, _>("status"), "accepted");
    assert_eq!(
        row.get::<String, _>("idempotency_key"),
        expected.to_string(),
        "the occurrence identifier is the operation's idempotency key",
    );
    assert_eq!(row.get::<Uuid, _>("message_id"), expected);
    assert_eq!(
        row.get::<String, _>("subject"),
        "cmd.github.sync.requested.v1"
    );

    let payload: serde_json::Value = row.get("payload");
    assert_eq!(payload["command_type"], "github.sync.requested.v1");
    assert_eq!(payload["payload"]["account"], "po4yka");
    assert_eq!(
        payload["tenant_id"],
        format!("user:{}", fixture.owner_user_id),
        "a scheduled command carries the schedule owner as its principal, not a system actor",
    );

    // The schedule moved forward, so a second pass at the same instant has nothing to do.
    assert!(next_due_at(pool, fixture.schedule_id).await > now);
    let second = run_once(pool, 32, now).await.expect("the pass must run");
    assert_eq!(second.due, 0);

    database.cleanup().await.expect("cleanup");
}

/// S-2. A disabled schedule is not due, however far past its due time it is.
#[tokio::test]
async fn a_disabled_schedule_is_never_due() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let mut fixture = Fixture::new(due);
    fixture.enabled = false;
    fixture.insert(pool).await;

    let report = run_once(pool, 32, due + SignedDuration::from_hours(24))
        .await
        .expect("the pass must run");
    assert_eq!(report.due, 0);
    assert_eq!(occurrence_count(pool, fixture.schedule_id).await, 0);
    assert_eq!(
        next_due_at(pool, fixture.schedule_id).await,
        due,
        "a disabled schedule is not silently advanced either",
    );

    database.cleanup().await.expect("cleanup");
}

/// S-3. A schedule whose next occurrence is in the future publishes nothing.
#[tokio::test]
async fn a_schedule_that_is_not_yet_due_publishes_nothing() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let fixture = Fixture::new(due);
    fixture.insert(pool).await;

    let report = run_once(pool, 32, due - SignedDuration::from_secs(1))
        .await
        .expect("the pass must run");
    assert_eq!(report.due, 0);
    assert_eq!(occurrence_count(pool, fixture.schedule_id).await, 0);

    database.cleanup().await.expect("cleanup");
}

/// S-4. The duplicate suppression S14 requires, exercised by moving `next_due_at` back to a due
/// time that has already been published. The occurrence is
/// recognised, NO second command is enqueued, and the schedule still moves forward — the last part
/// being what stops the pass from spinning on that row forever.
#[tokio::test]
async fn a_due_time_that_has_already_been_published_is_suppressed_and_still_advances() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let fixture = Fixture::new(due);
    fixture.insert(pool).await;

    let now = due + SignedDuration::from_secs(1);
    run_once(pool, 32, now).await.expect("the pass must run");
    assert_eq!(occurrence_count(pool, fixture.schedule_id).await, 1);

    sqlx::query("update operations.schedules set next_due_at = $2 where schedule_id = $1")
        .bind(fixture.schedule_id)
        .bind(to_offset(due))
        .execute(pool)
        .await
        .expect("the rewind must apply");

    let report = run_once(pool, 32, now).await.expect("the pass must run");
    assert_eq!((report.due, report.published, report.suppressed), (1, 0, 1));
    assert_eq!(occurrence_count(pool, fixture.schedule_id).await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from operations.outbox")
            .fetch_one(pool)
            .await
            .expect("the count must run"),
        1,
        "the suppressed occurrence must not enqueue a second command",
    );
    assert!(next_due_at(pool, fixture.schedule_id).await > now);

    database.cleanup().await.expect("cleanup");
}

/// S-5. A cron schedule advances to the next cron occurrence after its due instant.
#[tokio::test]
async fn cron_advances_from_the_due_occurrence() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let fixture = Fixture::new(due);
    fixture.insert(pool).await;

    let now = due + SignedDuration::from_secs(600);
    let report = run_once(pool, 32, now).await.expect("the pass must run");
    assert_eq!(report.published, 1);
    assert_eq!(occurrence_count(pool, fixture.schedule_id).await, 1);
    assert_eq!(
        next_due_at(pool, fixture.schedule_id).await,
        due + SignedDuration::from_secs(60)
    );

    database.cleanup().await.expect("cleanup");
}

/// S-6. A delayed cron schedule publishes each selected due occurrence once, each with its own
/// identifier.
#[tokio::test]
async fn cron_publishes_a_delayed_sequence_one_occurrence_per_pass() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let fixture = Fixture::new(due);
    fixture.insert(pool).await;

    let now = due + SignedDuration::from_secs(300);
    for expected in 1..=5_i64 {
        let report = run_once(pool, 32, now).await.expect("the pass must run");
        assert_eq!(report.published, 1, "pass {expected}");
        assert_eq!(occurrence_count(pool, fixture.schedule_id).await, expected);
    }

    // Five minutes of backlog on a one-minute schedule is five occurrences, and then it is level.
    let report = run_once(pool, 32, now).await.expect("the pass must run");
    assert_eq!(report.published, 1);
    assert_eq!(report.due, 1);
    let settled = run_once(pool, 32, now).await.expect("the pass must run");
    assert_eq!(settled.due, 0, "the schedule has caught up with now");

    let due_times: Vec<time::OffsetDateTime> = sqlx::query_scalar(
        "select due_at from operations.schedule_occurrences where schedule_id = $1 order by due_at",
    )
    .bind(fixture.schedule_id)
    .fetch_all(pool)
    .await
    .expect("the due times must read");
    assert_eq!(due_times.len(), 6);
    for (index, stored) in due_times.iter().enumerate() {
        let expected = due + SignedDuration::from_secs(60 * i64::try_from(index).unwrap());
        assert_eq!(from_offset(*stored), expected, "occurrence {index}");
    }

    database.cleanup().await.expect("cleanup");
}

/// S-7. The database refuses a schedule whose command type could never be published. The same
/// grammar guards `operations.outbox.subject`, and a row that can hold an arbitrary string is a row
/// that can address a subject outside the credential's allowlist.
#[tokio::test]
async fn a_command_type_outside_the_subject_grammar_is_refused_by_the_schema() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    for bad in [
        "github.sync.requested",        // no version
        "cmd.github.sync.requested.v1", // the class prefix belongs to the subject, not the type
        "Github.Sync.Requested.v1",
        "github.sync.requested.v0",
    ] {
        let outcome = sqlx::query(
            "insert into operations.schedules
                 (schedule_id, service_name, name, owner_user_id, command_type, operation_kind,
                  cron_expression, next_due_at, created_at, updated_at)
             values ($1, 'ratatoskr-github', $2, $3, $4, 'github.sync', '* * * * *', now(), now(), now())",
        )
        .bind(Uuid::now_v7())
        .bind(
            format!("n{}", Uuid::now_v7().simple())
                .chars()
                .take(60)
                .collect::<String>(),
        )
        .bind(Uuid::now_v7())
        .bind(bad)
        .execute(pool)
        .await;
        assert!(outcome.is_err(), "`{bad}` must be refused");
    }

    database.cleanup().await.expect("cleanup");
}

/// S-8. Two schedules due at the same instant are both published in one pass, and neither takes the
/// other's identifier. The lease is per schedule, not per process.
#[tokio::test]
async fn two_schedules_due_at_once_are_both_published() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let first = Fixture::new(due);
    let second = Fixture::new(due);
    first.insert(pool).await;
    second.insert(pool).await;

    let report = run_once(pool, 32, due).await.expect("the pass must run");
    assert_eq!((report.due, report.published), (2, 2));
    assert_eq!(occurrence_count(pool, first.schedule_id).await, 1);
    assert_eq!(occurrence_count(pool, second.schedule_id).await, 1);
    assert_ne!(
        occurrence_id(first.schedule_id, due),
        occurrence_id(second.schedule_id, due),
    );

    database.cleanup().await.expect("cleanup");
}

/// S-9. Retention removes old occurrence records and leaves recent ones.
///
/// The window is also how far back an operator may rewind `next_due_at` before a rewind republishes
/// instead of being suppressed, which is why it is ninety days by default and not ninety minutes.
/// Nothing else depends on it: `next_due_at` only ever moves forward under its own power.
#[tokio::test]
async fn retention_removes_occurrences_outside_the_window() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();

    let due = at(1_700_000_000);
    let fixture = Fixture::new(due);
    fixture.insert(pool).await;

    // Three occurrences, published a minute apart at a due time far in the past.
    let now = due + SignedDuration::from_secs(180);
    for _ in 0..3 {
        run_once(pool, 32, now).await.expect("the pass must run");
    }
    assert_eq!(occurrence_count(pool, fixture.schedule_id).await, 3);

    // A window that ends before all of them, then one that ends after.
    let removed = platform_scheduling::collect_occurrences_before(
        pool,
        due - SignedDuration::from_hours(1),
        1000,
    )
    .await
    .expect("collecting");
    assert_eq!(removed, 0, "nothing published after the cut-off may go");

    let removed = platform_scheduling::collect_occurrences_before(
        pool,
        now + SignedDuration::from_secs(1),
        1000,
    )
    .await
    .expect("collecting");
    assert_eq!(removed, 3);
    assert_eq!(occurrence_count(pool, fixture.schedule_id).await, 0);

    database.cleanup().await.expect("cleanup");
}

/// A registered channel-digest schedule: the one command type Platform publishes as a contract
/// envelope instead of the legacy document.
async fn insert_digest_schedule(
    pool: &PgPool,
    schedule_id: Uuid,
    owner_user_id: Uuid,
    cron: &str,
    next_due_at: Timestamp,
) {
    let created = to_offset(next_due_at - SignedDuration::from_hours(1));
    sqlx::query(
        "insert into operations.schedules
             (schedule_id, service_name, name, owner_user_id, command_type, operation_kind, payload,
              cron_expression, next_due_at, enabled, created_at, updated_at)
         values ($1, 'ratatoskr-channel-digests', 'daily-digest', $2,
                 'channel_digest.schedule.occurrence_requested.v1',
                 'channel_digest.schedule.occurrence', '{}'::jsonb, $3, $4, true, $5, $5)",
    )
    .bind(schedule_id)
    .bind(owner_user_id)
    .bind(cron)
    .bind(to_offset(next_due_at))
    .bind(created)
    .execute(pool)
    .await
    .expect("the digest schedule must insert");
}

async fn digest_command(
    pool: &PgPool,
    schedule_id: Uuid,
    due: Timestamp,
) -> (Uuid, serde_json::Value) {
    let expected = occurrence_id(schedule_id, due);
    let row = sqlx::query(
        "select b.message_id, b.subject, b.payload, o.operation_id
           from operations.schedule_occurrences o
           join operations.outbox b on b.operation_id = o.operation_id
          where o.occurrence_id = $1",
    )
    .bind(expected)
    .fetch_one(pool)
    .await
    .expect("the occurrence and its command must exist");
    assert_eq!(
        row.get::<String, _>("subject"),
        "cmd.channel_digest.schedule.occurrence_requested.v1"
    );
    (row.get("operation_id"), row.get("payload"))
}

/// S08. The digest occurrence is a typed `CommandEnvelope`: its command id is the occurrence id, its
/// aggregate is the occurrence reference, and the interval is the grid, not the clock.
#[tokio::test]
async fn digest_occurrence_is_published_as_a_contract_envelope() {
    use ratatoskr_channel_digest_contracts::ChannelDigestScheduleOccurrenceRequested;
    use ratatoskr_event_envelope::CommandEnvelope;

    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();
    let due: Timestamp = "2026-10-10T06:00:00Z".parse().expect("a timestamp");
    let schedule_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    insert_digest_schedule(pool, schedule_id, owner, "0 6 * * *", due).await;

    let report = run_once(pool, 32, due + SignedDuration::from_secs(7))
        .await
        .expect("the pass must run");
    assert_eq!((report.due, report.published), (1, 1));

    let occurrence = occurrence_id(schedule_id, due);
    let (operation_id, stored) = digest_command(pool, schedule_id, due).await;
    let envelope = CommandEnvelope::from_json(stored.to_string().as_bytes())
        .expect("the stored row is a contract CommandEnvelope");
    let payload: ChannelDigestScheduleOccurrenceRequested = envelope
        .payload_as()
        .expect("the payload is an occurrence request");
    payload
        .validate_for_publish()
        .expect("a publishable command");

    assert_eq!(stored["command_id"], occurrence.to_string());
    assert_eq!(stored["producer"], "ratatoskr-platform");
    assert_eq!(
        stored["aggregate_id"],
        format!("schedule-occurrence:{occurrence}")
    );
    assert_eq!(
        stored["payload"]["occurrence_ref"],
        format!("schedule-occurrence:{occurrence}"),
        "the aggregate equals the occurrence reference"
    );
    assert_eq!(
        stored["correlation_id"],
        format!("operation:{operation_id}")
    );
    assert_eq!(stored["tenant_id"], format!("user:{owner}"));
    assert_eq!(
        stored["payload"]["schedule_ref"],
        format!("schedule:{schedule_id}")
    );
    assert_eq!(
        payload.due_at.as_jiff(),
        due,
        "the grid point, not the clock"
    );
    assert_eq!(
        payload.previous_due_at.as_jiff(),
        due - SignedDuration::from_hours(24),
        "the previous grid point of a daily schedule"
    );
    database.cleanup().await.expect("cleanup");
}

/// S08. The window a digest can cover is seven days, so a sparse schedule's "previous" grid point is
/// clamped rather than producing a command the consumer must refuse.
#[tokio::test]
async fn previous_due_at_is_clamped_to_seven_days_for_a_sparse_cron() {
    use ratatoskr_channel_digest_contracts::ChannelDigestScheduleOccurrenceRequested;
    use ratatoskr_event_envelope::CommandEnvelope;

    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();
    let due: Timestamp = "2027-01-01T00:00:00Z".parse().expect("a timestamp");
    let schedule_id = Uuid::now_v7();
    insert_digest_schedule(pool, schedule_id, Uuid::now_v7(), "0 0 1 1 *", due).await;

    run_once(pool, 32, due + SignedDuration::from_secs(1))
        .await
        .expect("the pass must run");

    let (_, stored) = digest_command(pool, schedule_id, due).await;
    let payload: ChannelDigestScheduleOccurrenceRequested =
        CommandEnvelope::from_json(stored.to_string().as_bytes())
            .expect("an envelope")
            .payload_as()
            .expect("an occurrence request");
    payload
        .validate_for_publish()
        .expect("a publishable command");
    assert_eq!(
        payload.previous_due_at.as_jiff(),
        due - SignedDuration::from_hours(7 * 24)
    );
    database.cleanup().await.expect("cleanup");
}

/// S08. Every other command type keeps the legacy document, member for member.
#[tokio::test]
async fn a_github_schedule_keeps_the_legacy_command_document() {
    let database = TestDatabase::create().await.expect("a test database");
    let pool = database.pool();
    let due = at(1_700_000_000);
    let fixture = Fixture::new(due);
    fixture.insert(pool).await;

    run_once(pool, 32, due + SignedDuration::from_secs(3))
        .await
        .expect("the pass must run");
    let stored: serde_json::Value =
        sqlx::query_scalar("select payload from operations.outbox where message_id = $1")
            .bind(occurrence_id(fixture.schedule_id, due))
            .fetch_one(pool)
            .await
            .expect("the command");
    let mut members: Vec<&str> = stored
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    members.sort_unstable();
    assert_eq!(
        members,
        [
            "command_id",
            "command_type",
            "correlation_id",
            "idempotency_key",
            "operation_id",
            "payload",
            "requested_at",
            "tenant_id",
        ]
    );
    database.cleanup().await.expect("cleanup");
}
