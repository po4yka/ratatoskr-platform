//! The stale-operation reaper, against a real database.
//!
//! Every claim worth making here is about a predicate, a lock or a constraint, none of which a
//! unit test observes: liveness is an aggregate over two tables, the race is closed by a row lock
//! inside the terminating transaction, and the contract invariant "a failed operation carries an
//! error" is checked by `OperationSnapshot::validate` against what the database actually holds.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use jiff::{SignedDuration, Timestamp};
use platform_eventing::Handler as _;
use platform_eventing::{Incoming, MessageClass, Subject};
use platform_persistence::test_support::TestDatabase;
use ratatoskr_identifiers::{Extensions, OperationId};
use ratatoskr_operation_contracts::{OperationReported, OperationStatus};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";

/// The instant every fixture is anchored at, so ages read as arithmetic in the tests.
const T0: i64 = 1_700_000_000;

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_second(seconds).expect("a timestamp inside the supported range")
}

fn to_offset(value: Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .expect("a timestamp inside the supported range")
}

fn from_offset(value: time::OffsetDateTime) -> Timestamp {
    Timestamp::from_nanosecond(value.unix_timestamp_nanos())
        .expect("a timestamp inside the supported range")
}

/// An operation seeded straight into the store, bypassing `accept`, so a test controls its age.
struct Fixture {
    operation_id: Uuid,
    status: &'static str,
    /// When its status last changed.
    changed_at: Timestamp,
}

impl Fixture {
    /// An unterminated operation whose only fact is a status change at `changed_at`.
    fn silent(operation_id: Uuid, changed_at: Timestamp) -> Self {
        Self {
            operation_id,
            status: "accepted",
            changed_at,
        }
    }

    async fn seed(&self, pool: &PgPool) {
        sqlx::query(
            "insert into operations.operations
                 (operation_id, owner_user_id, kind, status, correlation_id,
                  accepted_at, status_changed_at)
             values ($1, $2, 'content.capture.submit', $3, $4, $5, $5)",
        )
        .bind(self.operation_id)
        .bind(Uuid::now_v7())
        .bind(self.status)
        .bind(CORRELATION)
        .bind(to_offset(self.changed_at))
        .execute(pool)
        .await
        .expect("seeding an operation");
    }

    /// One observed fact after the status change: a progress entry at `observed_at`.
    async fn observed_at(&self, pool: &PgPool, observed_at: Timestamp) -> &Self {
        sqlx::query(
            "insert into operations.operation_progress
                 (progress_id, operation_id, observed_at, status)
             values ($1, $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(self.operation_id)
        .bind(to_offset(observed_at))
        .bind(self.status)
        .execute(pool)
        .await
        .expect("seeding a progress entry");
        self
    }
}

async fn status_of(pool: &PgPool, operation_id: Uuid) -> String {
    sqlx::query_scalar("select status from operations.operations where operation_id = $1")
        .bind(operation_id)
        .fetch_one(pool)
        .await
        .expect("the operation must exist")
}

async fn terminated_at_of(pool: &PgPool, operation_id: Uuid) -> Option<Timestamp> {
    let stored: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "select terminated_at from operations.operations where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .expect("the operation must exist");
    stored.map(from_offset)
}

async fn error_row_of(pool: &PgPool, operation_id: Uuid) -> (String, String, bool, String) {
    let row =
        sqlx::query("select code, message, retryable, severity from operations.operation_errors where operation_id = $1")
            .bind(operation_id)
            .fetch_one(pool)
            .await
            .expect("the reconciled operation must carry exactly one error record");
    (
        row.get("code"),
        row.get("message"),
        row.get("retryable"),
        row.get("severity"),
    )
}

/// R-1. A silent operation is failed with its error, and the termination is visible on the
/// surfaces clients read.
///
/// The error record is not decoration: `OperationSnapshot::validate` refuses a `failed` operation
/// with no error (invariant I2), so a reaper that only flipped the status column would make every
/// subsequent poll of that operation a contract violation.
#[tokio::test]
async fn a_silent_operation_is_failed_with_its_error() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let fixture = Fixture::silent(Uuid::now_v7(), at(T0));
    fixture.seed(pool).await;

    let now = at(T0) + SignedDuration::from_hours(25);
    let window = SignedDuration::from_hours(24);
    let report = platform_operations::reconcile::run_once(pool, window, 100, now)
        .await
        .expect("the pass must run");
    assert_eq!(
        report.reconciled, 1,
        "the silent operation must be harvested"
    );
    assert_eq!(report.skipped, 0);

    assert_eq!(status_of(pool, fixture.operation_id).await, "failed");
    let terminated = terminated_at_of(pool, fixture.operation_id)
        .await
        .expect("a reconciled operation must be terminated");
    assert_eq!(
        terminated, now,
        "terminated at the pass instant, not the last activity"
    );

    let (code, message, retryable, severity) = error_row_of(pool, fixture.operation_id).await;
    assert_eq!(code, "platform.operation.stale");
    assert!(
        retryable,
        "an operation the platform terminated for silence may be resubmitted"
    );
    assert_eq!(severity, "error");
    assert!(
        !message.contains('\n'),
        "the message must be a safe single-line string"
    );
    assert!(message.len() <= 200);

    let stored: bool =
        sqlx::query_scalar("select retryable from operations.operations where operation_id = $1")
            .bind(fixture.operation_id)
            .fetch_one(pool)
            .await
            .expect("the operation must exist");
    assert!(
        stored,
        "the operation's own retryable flag must match its error record"
    );

    let progress: Vec<(String, Option<String>)> = sqlx::query(
        "select status, message from operations.operation_progress where operation_id = $1",
    )
    .bind(fixture.operation_id)
    .map(|row: sqlx::postgres::PgRow| {
        (
            row.get::<String, _>("status"),
            row.get::<Option<String>, _>("message"),
        )
    })
    .fetch_all(pool)
    .await
    .expect("the progress history must read");
    assert!(
        progress.iter().any(|(status, _)| status == "failed"),
        "the termination must be in the history a client replays, got {progress:?}"
    );

    harness.cleanup().await.expect("cleanup");
}

/// R-2. Liveness is the newest observed fact, and a progress entry is a fact.
///
/// `status_changed_at` moves only on an applied ADVANCE, so a worker that reports progress every
/// minute without changing status is alive however long the work takes.
#[tokio::test]
async fn an_operation_that_reported_inside_the_window_is_never_harvested() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let fixture = Fixture::silent(Uuid::now_v7(), at(T0));
    fixture.seed(pool).await;
    // A status change an hour before the cutoff would be stale on its own; the report inside the
    // window is what keeps it alive.
    fixture
        .observed_at(
            pool,
            at(T0) + SignedDuration::from_hours(24) - SignedDuration::from_secs(60),
        )
        .await;

    let now = at(T0) + SignedDuration::from_hours(25);
    let window = SignedDuration::from_hours(24);
    let report = platform_operations::reconcile::run_once(pool, window, 100, now)
        .await
        .expect("the pass must run");
    assert_eq!(report.reconciled, 0);
    assert_eq!(status_of(pool, fixture.operation_id).await, "accepted");
    assert!(terminated_at_of(pool, fixture.operation_id).await.is_none());

    harness.cleanup().await.expect("cleanup");
}

/// R-3. A report arriving after reconciliation is ordinary stale traffic (ADR-0002's rank rule),
/// not a resurrection: the operation stays failed with its error intact.
#[tokio::test]
async fn a_late_report_after_reconciliation_does_not_resurrect() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let fixture = Fixture::silent(Uuid::now_v7(), at(T0));
    fixture.seed(pool).await;
    platform_operations::reconcile::run_once(
        pool,
        SignedDuration::from_hours(24),
        100,
        at(T0) + SignedDuration::from_hours(25),
    )
    .await
    .expect("the pass must run");

    let message = Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::new(MessageClass::Event, "platform.operation.reported.v1")
            .expect("a subject"),
        producer: "ratatoskr-extractor".to_owned(),
        payload: serde_json::json!({
            "event_id": Uuid::now_v7(),
            "producer": "ratatoskr-extractor",
            "payload": serde_json::to_value(OperationReported {
                operation_id: OperationId(fixture.operation_id),
                status: OperationStatus::Running,
                stage: None,
                progress_percent: None,
                results: Vec::new(),
                error: None,
                warnings: Vec::new(),
                extensions: Extensions::default(),
            })
            .expect("the published payload serializes"),
        }),
    };

    let mut transaction = pool.begin().await.expect("a transaction");
    let outcome = platform_operations::ProgressProjection
        .handle(&mut transaction, &message)
        .await
        .expect("applying a valid message");
    transaction.commit().await.expect("committing");
    assert_eq!(
        outcome,
        platform_eventing::inbox::Outcome::Stale,
        "a late running after a terminal status is stale traffic"
    );

    assert_eq!(status_of(pool, fixture.operation_id).await, "failed");
    let (_, _, retryable, _) = error_row_of(pool, fixture.operation_id).await;
    assert!(retryable, "the original error record must survive");

    harness.cleanup().await.expect("cleanup");
}

/// R-4. Idempotence: a second pass over reconciled rows changes nothing.
#[tokio::test]
async fn two_passes_do_not_double_terminate() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let fixture = Fixture::silent(Uuid::now_v7(), at(T0));
    fixture.seed(pool).await;
    let now = at(T0) + SignedDuration::from_hours(25);
    let window = SignedDuration::from_hours(24);

    let first = platform_operations::reconcile::run_once(pool, window, 100, now)
        .await
        .expect("the pass must run");
    assert_eq!(first.reconciled, 1);

    let second = platform_operations::reconcile::run_once(pool, window, 100, now)
        .await
        .expect("the pass must run");
    assert_eq!(second.reconciled, 0, "nothing is left to terminate");
    assert_eq!(second.skipped, 0);

    let errors: i64 = sqlx::query_scalar::<_, i64>(
        "select count(*) from operations.operation_errors where operation_id = $1",
    )
    .bind(fixture.operation_id)
    .fetch_one(pool)
    .await
    .expect("the count must run");
    assert_eq!(errors, 1, "one termination, one error record");

    harness.cleanup().await.expect("cleanup");
}

/// R-5. A bounded pass terminates at most its batch, oldest first; the rest wait for later passes.
#[tokio::test]
async fn a_bounded_pass_terminates_at_most_the_batch_oldest_first() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    // Five silent operations, one hour apart. The batch of three must take the three oldest.
    let mut fixtures = Vec::new();
    for hours in [5_i64, 4, 3, 2, 1] {
        let fixture = Fixture::silent(
            Uuid::now_v7(),
            at(T0) + SignedDuration::from_hours(25 - hours),
        );
        fixture.seed(pool).await;
        fixtures.push((fixture, hours));
    }

    let now = at(T0) + SignedDuration::from_hours(50);
    let window = SignedDuration::from_hours(24);
    let report = platform_operations::reconcile::run_once(pool, window, 3, now)
        .await
        .expect("the pass must run");
    assert_eq!(report.reconciled, 3, "the batch bound holds");

    for (fixture, age_hours) in &fixtures {
        let terminated = terminated_at_of(pool, fixture.operation_id).await.is_some();
        if *age_hours >= 3 {
            assert!(
                terminated,
                "the {age_hours}-hour-old operation is among the three oldest"
            );
        } else {
            assert!(
                !terminated,
                "the {age_hours}-hour-old operation waits for a later pass"
            );
        }
    }

    let remainder = platform_operations::reconcile::run_once(pool, window, 100, now)
        .await
        .expect("the pass must run");
    assert_eq!(remainder.reconciled, 2, "later passes drain the remainder");

    harness.cleanup().await.expect("cleanup");
}

/// R-6. What WE moved is counted where we move it, separately from the transition counter: a
/// misbehaving worker and an aggressive window must stay distinguishable.
///
/// A plain `#[test]`, because the counting recorder is thread-local and the pass must therefore
/// run on the thread that installed it.
#[test]
fn reconciliations_are_counted_on_the_metrics_surface() {
    #[derive(Clone, Default)]
    struct Recorded(Arc<std::sync::Mutex<Vec<String>>>);

    impl Recorded {
        fn count(&self, series: &str) -> usize {
            self.0
                .lock()
                .expect("the test recorder is uncontended")
                .iter()
                .filter(|entry| entry.as_str() == series)
                .count()
        }
    }

    struct CountingRecorder(Recorded);

    impl metrics::Recorder for CountingRecorder {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }

        fn register_counter(
            &self,
            key: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            let recorded = self.0.clone();
            let series = key.name().to_owned();
            metrics::Counter::from_arc(Arc::new(RecordingCounter { series, recorded }))
        }

        fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            metrics::Gauge::noop()
        }

        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    struct RecordingCounter {
        series: String,
        recorded: Recorded,
    }

    impl metrics::CounterFn for RecordingCounter {
        fn increment(&self, value: u64) {
            let mut entries = self.recorded.0.lock().expect("uncontended");
            for _ in 0..value {
                entries.push(self.series.clone());
            }
        }

        fn absolute(&self, value: u64) {
            self.increment(value);
        }
    }

    let recorded = Recorded::default();
    let counted = recorded.clone();

    metrics::with_local_recorder(&CountingRecorder(counted), || {
        // A current-thread runtime inside the closure, because the recorder is thread-local and
        // the pass must be counted on the thread that installed it.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a single-threaded runtime")
            .block_on(async {
                let harness = TestDatabase::create().await.expect("a test database");
                let pool = harness.pool();

                for _ in 0..2 {
                    Fixture::silent(Uuid::now_v7(), at(T0)).seed(pool).await;
                }

                platform_operations::reconcile::run_once(
                    pool,
                    SignedDuration::from_hours(24),
                    100,
                    at(T0) + SignedDuration::from_hours(25),
                )
                .await
                .expect("the pass must run");

                assert_eq!(
                    recorded.count("platform_operations_reconciled_total"),
                    2,
                    "one increment per terminated operation",
                );

                harness.cleanup().await.expect("cleanup");
            });
    });
}
