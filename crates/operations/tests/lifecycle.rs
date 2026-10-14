//! The operations schema and its state machine, against a real `PostgreSQL`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_operations::OperationError;
use platform_operations::transition::{self, Transition};
use platform_persistence::test_support::TestDatabase;
use ratatoskr_identifiers::{EntityRef, Extensions};
use ratatoskr_operation_contracts::{OperationResultKind, OperationResultRef, OperationStatus};
use uuid::Uuid;

/// The instant the whole file runs at, so no assertion depends on the day the suite runs.
fn now() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).expect("a fixed timestamp")
}

fn owner() -> Uuid {
    Uuid::now_v7()
}

const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";

/// L-1. The Rust transition table and the SQL trigger agree on all 49 ordered pairs.
///
/// This is the test that makes two enforcement points safe to have. It does not compare source
/// text: it asks the database to perform every transition and compares the outcome with
/// `status::may_transition`. A disagreement in either direction fails.
#[tokio::test]
async fn the_sql_trigger_and_the_rust_table_agree_on_every_pair() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for from in transition::ALL {
        for to in transition::ALL {
            let operation_id = Uuid::now_v7();
            let terminal_from = transition::is_terminal(from);

            // Seed the row directly in the `from` status. The trigger only fires on UPDATE, so an
            // INSERT can place an operation in any legal starting state.
            sqlx::query(
                "insert into operations.operations
                     (operation_id, owner_user_id, kind, status, correlation_id,
                      accepted_at, status_changed_at, terminated_at)
                 values ($1, $2, 'content.capture.submit', $3, $4, now(), now(),
                         case when $5 then now() end)",
            )
            .bind(operation_id)
            .bind(owner())
            .bind(status_token(from))
            .bind(CORRELATION)
            // The database's clock, not this process's. `now()` is the transaction timestamp, and a
            // Rust clock read taken microseconds earlier lands BEFORE it, which the
            // `terminated_at_is_not_before_accepted_at` constraint correctly rejects.
            .bind(terminal_from)
            .execute(pool)
            .await
            .expect("seeding an operation");

            let terminal_to = transition::is_terminal(to);
            let outcome = sqlx::query(
                "update operations.operations
                    set status = $2, status_changed_at = now(),
                        terminated_at = case when $3 then now() end
                  where operation_id = $1",
            )
            .bind(operation_id)
            .bind(status_token(to))
            .bind(terminal_to)
            .execute(pool)
            .await;

            let sql_allowed = outcome.is_ok();
            // The trigger guards WRITES. `record_status` never issues one for `Duplicate` or
            // `Stale`, so the set the trigger must accept is exactly the set that reaches it: an
            // `Advance`, plus the same-status write a `Duplicate` would be if one were attempted.
            let rust_allows_write = matches!(
                transition::apply(from, to),
                Transition::Advance(_) | Transition::Duplicate
            );

            assert_eq!(
                sql_allowed, rust_allows_write,
                "{from:?} -> {to:?}: the trigger says {sql_allowed}, the transition table says \
                 {rust_allows_write}. The two enforcement points must never disagree."
            );
        }
    }

    harness.cleanup().await.expect("dropping the test database");
}

/// L-2. The happy path, through the public API of the crate.
#[tokio::test]
async fn an_operation_runs_from_acceptance_to_success() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = owner();

    let accepted = platform_operations::accept(
        pool,
        owner,
        "content.capture.submit",
        CORRELATION,
        Some("idem-1"),
        None,
        now(),
    )
    .await
    .expect("accepting");
    assert_eq!(accepted.status, OperationStatus::Accepted);
    assert!(accepted.terminated_at.is_none());

    for status in [
        OperationStatus::Queued,
        OperationStatus::Running,
        OperationStatus::Succeeded,
    ] {
        let mut transaction = pool.begin().await.expect("a transaction");
        let (outcome, _) = platform_operations::record_status(
            &mut transaction,
            accepted.operation_id,
            status,
            Some("downloading"),
            Some(50),
            Some("still going"),
            now(),
        )
        .await
        .expect("a legal transition");
        assert_eq!(outcome, Transition::Advance(status));
        transaction.commit().await.expect("commit");
    }

    let finished = platform_operations::find(pool, accepted.operation_id)
        .await
        .expect("reading")
        .expect("the operation");
    assert_eq!(finished.status, OperationStatus::Succeeded);
    assert!(
        finished.terminated_at.is_some(),
        "a terminal operation must carry a termination instant"
    );

    // The progress history recorded every move, which is what an SSE reconnect replays.
    let progress: i64 = sqlx::query_scalar(
        "select count(*) from operations.operation_progress where operation_id = $1",
    )
    .bind(accepted.operation_id)
    .fetch_one(pool)
    .await
    .expect("counting progress");
    assert_eq!(progress, 3);

    harness.cleanup().await.expect("dropping the test database");
}

/// L-3. A late older status is ordinary traffic. ADR-0002 classifies it as `Stale`: nothing is
/// written, no error is raised, and the caller counts it. Treating it as a failure would make
/// at-least-once redelivery look like a fault.
#[tokio::test]
async fn a_late_older_status_is_stale_and_writes_nothing() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let operation = platform_operations::accept(
        pool,
        owner(),
        "content.capture.submit",
        CORRELATION,
        None,
        None,
        now(),
    )
    .await
    .expect("accepting");

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_operations::record_status(
        &mut transaction,
        operation.operation_id,
        OperationStatus::Running,
        None,
        None,
        None,
        now(),
    )
    .await
    .expect("accepted -> running advances");

    let (outcome, unchanged) = platform_operations::record_status(
        &mut transaction,
        operation.operation_id,
        OperationStatus::Queued,
        None,
        None,
        None,
        now(),
    )
    .await
    .expect("a stale delivery is not an error");
    assert_eq!(outcome, Transition::Stale);
    assert_eq!(unchanged.status, OperationStatus::Running);

    // Re-delivering the current status is a duplicate, also a silent no-op.
    let (duplicate, _) = platform_operations::record_status(
        &mut transaction,
        operation.operation_id,
        OperationStatus::Running,
        None,
        None,
        None,
        now(),
    )
    .await
    .expect("a duplicate is not an error");
    assert_eq!(duplicate, Transition::Duplicate);

    transaction.commit().await.expect("commit");

    // Exactly one progress row: the advance. Neither no-op appended history.
    let progress: i64 = sqlx::query_scalar(
        "select count(*) from operations.operation_progress where operation_id = $1",
    )
    .bind(operation.operation_id)
    .fetch_one(pool)
    .await
    .expect("counting");
    assert_eq!(progress, 1);

    harness.cleanup().await.expect("dropping the test database");
}

/// L-3b. Two different terminal outcomes are the one case that is an alarm.
#[tokio::test]
async fn two_terminal_outcomes_conflict() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let operation = platform_operations::accept(
        pool,
        owner(),
        "content.capture.submit",
        CORRELATION,
        None,
        None,
        now(),
    )
    .await
    .expect("accepting");

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_operations::record_status(
        &mut transaction,
        operation.operation_id,
        OperationStatus::Succeeded,
        None,
        None,
        None,
        now(),
    )
    .await
    .expect("succeeding");

    let conflict = platform_operations::record_status(
        &mut transaction,
        operation.operation_id,
        OperationStatus::Failed,
        None,
        None,
        None,
        now(),
    )
    .await;
    assert!(
        matches!(conflict, Err(OperationError::ConflictingOutcome { .. })),
        "expected a conflict, got {conflict:?}"
    );

    transaction.rollback().await.expect("rollback");
    harness.cleanup().await.expect("dropping the test database");
}

/// L-4. The idempotency scope is enforced by the schema, per actor and kind.
#[tokio::test]
async fn one_idempotency_key_accepts_one_operation_per_actor_and_kind() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = owner();

    platform_operations::accept(
        pool,
        owner,
        "content.capture.submit",
        CORRELATION,
        Some("k"),
        None,
        now(),
    )
    .await
    .expect("the first");

    let second = platform_operations::accept(
        pool,
        owner,
        "content.capture.submit",
        CORRELATION,
        Some("k"),
        None,
        now(),
    )
    .await;
    assert!(
        second.is_err(),
        "a repeated key must not accept a second operation"
    );

    // A different kind is a different scope, so the same key is free again.
    platform_operations::accept(
        pool,
        owner,
        "social.source.sync",
        CORRELATION,
        Some("k"),
        None,
        now(),
    )
    .await
    .expect("a different kind is a different scope");

    harness.cleanup().await.expect("dropping the test database");
}

/// L-5. The projection produces a contract-valid snapshot, and refuses to produce an invalid one.
#[tokio::test]
async fn the_snapshot_projection_satisfies_the_contract() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let operation = platform_operations::accept(
        pool,
        owner(),
        "content.capture.submit",
        CORRELATION,
        None,
        None,
        now(),
    )
    .await
    .expect("accepting");

    platform_operations::record_result(
        pool,
        operation.operation_id,
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

    let snapshot = platform_operations::snapshot(pool, operation.operation_id)
        .await
        .expect("projecting");
    assert_eq!(snapshot.status, OperationStatus::Accepted);
    assert_eq!(snapshot.results.len(), 1);
    snapshot
        .validate()
        .expect("the projection must be contract-valid");

    // A `failed` operation with no recorded error violates contract invariant I2, and the
    // projection must refuse rather than emit it.
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_operations::record_status(
        &mut transaction,
        operation.operation_id,
        OperationStatus::Failed,
        None,
        None,
        None,
        now(),
    )
    .await
    .expect("running the transition");
    transaction.commit().await.expect("commit");

    let refused = platform_operations::snapshot(pool, operation.operation_id).await;
    assert!(
        matches!(refused, Err(OperationError::ContractViolation(_))),
        "a failed operation with no error must not project, got {refused:?}"
    );

    harness.cleanup().await.expect("dropping the test database");
}

/// L-6. Two writers that fail one operation at the same time record one terminal transition and
/// one error.
///
/// Each writer does what the Edge failure paths do: it applies `failed`, and it records an error
/// only when that was an `Advance`. The second writer starts while the first one holds the row and
/// has not committed. If `record_status` reads the status without a row lock, both writers read
/// `accepted`, both advance, and the operation holds two `failed` progress entries and two errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_failures_record_one_transition_and_one_error() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation_id = platform_operations::accept(
        pool,
        owner(),
        "content.capture.submit",
        CORRELATION,
        None,
        None,
        now(),
    )
    .await
    .expect("accepting")
    .operation_id;

    let mut first = pool.begin().await.expect("the first transaction");
    let first_outcome = fail_once(&mut first, operation_id).await;
    assert_eq!(first_outcome, Transition::Advance(OperationStatus::Failed));

    let second = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut transaction = pool.begin().await.expect("the second transaction");
            let outcome = fail_once(&mut transaction, operation_id).await;
            transaction.commit().await.expect("the second commit");
            outcome
        })
    };

    // The first writer commits only when the second one waits for a lock, so the second one has
    // started its work before the first one ended.
    let mut observer = <sqlx::PgConnection as sqlx::Connection>::connect(&harness.url())
        .await
        .expect("an observer connection");
    let waiting = async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity
                  where datname = current_database() and wait_event_type = 'Lock'",
            )
            .fetch_one(&mut observer)
            .await
            .expect("reading the lock waits");
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .expect("the second writer waits for the row of the first one");
    first.commit().await.expect("the first commit");
    let second_outcome = second.await.expect("the second writer");

    let failed: i64 = sqlx::query_scalar(
        "select count(*) from operations.operation_progress
          where operation_id = $1 and status = 'failed'",
    )
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .expect("counting the failed entries");
    let errors: i64 = sqlx::query_scalar(
        "select count(*) from operations.operation_errors where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .expect("counting the errors");
    assert_eq!(
        (second_outcome, failed, errors),
        (Transition::Duplicate, 1, 1),
        "the second writer must find the operation failed: (outcome, failed entries, errors)"
    );

    harness.cleanup().await.expect("dropping the test database");
}

/// Apply `failed` and, when that advanced the operation, record its error: the pattern of
/// `fail_notes_connect` and `fail_ai_archive_delivery`, inside the caller's transaction.
async fn fail_once(transaction: &mut sqlx::PgTransaction<'_>, operation_id: Uuid) -> Transition {
    let (outcome, _) = platform_operations::record_status(
        transaction,
        operation_id,
        OperationStatus::Failed,
        Some("connect"),
        None,
        Some("The connect did not complete."),
        now(),
    )
    .await
    .expect("applying failed");
    if matches!(outcome, Transition::Advance(_)) {
        let error = ratatoskr_error_contracts::ErrorEnvelope::new(
            ratatoskr_error_contracts::ErrorCode::parse("edge.upstream_unavailable")
                .expect("a stable code"),
            ratatoskr_identifiers::SafeMessage::parse("The connect did not complete.")
                .expect("a safe message"),
            true,
        );
        platform_operations::record_error(transaction, operation_id, &error, now())
            .await
            .expect("recording the error");
    }
    outcome
}

fn status_token(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Accepted => "accepted",
        OperationStatus::Queued => "queued",
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::PartiallySucceeded => "partially_succeeded",
        OperationStatus::Failed => "failed",
        OperationStatus::Cancelled => "cancelled",
        _ => unreachable!("transition::ALL contains only the seven known variants"),
    }
}
