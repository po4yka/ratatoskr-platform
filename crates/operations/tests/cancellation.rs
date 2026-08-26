//! Cancellation and owner-scoped listing, against a real `PostgreSQL`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Arc;

use platform_operations::Cancellation;
use platform_operations::OperationError;
use platform_operations::transition::{self, Transition};
use platform_persistence::test_support::TestDatabase;
use ratatoskr_operation_contracts::OperationStatus;
use uuid::Uuid;

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn owner() -> Uuid {
    Uuid::now_v7()
}

const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";

/// Seed an operation directly in the given status. The trigger only fires on UPDATE, so an
/// INSERT can stage any legal starting state, exactly as in `lifecycle.rs`.
async fn seeded_in_status(pool: &sqlx::PgPool, owner_id: Uuid, status: OperationStatus) -> Uuid {
    let operation_id = Uuid::now_v7();
    let token = match status {
        OperationStatus::Accepted => "accepted",
        OperationStatus::Queued => "queued",
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::PartiallySucceeded => "partially_succeeded",
        OperationStatus::Failed => "failed",
        OperationStatus::Cancelled => "cancelled",
        _ => unreachable!("transition::ALL contains only the seven known variants"),
    };
    sqlx::query(
        "insert into operations.operations
             (operation_id, owner_user_id, kind, status, correlation_id,
              accepted_at, status_changed_at, terminated_at)
         values ($1, $2, 'content.capture.submit', $3, $4, now(), now(),
                 case when $5 then now() end)",
    )
    .bind(operation_id)
    .bind(owner_id)
    .bind(token)
    .bind(CORRELATION)
    .bind(transition::is_terminal(status))
    .execute(pool)
    .await
    .expect("seeding an operation");
    operation_id
}

/// The cancellation marker of one row, as seen by the given connection or transaction.
async fn marked<'e, E>(executor: E, operation_id: Uuid) -> bool
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar(
        "select cancellation_requested_at is not null
           from operations.operations where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(executor)
    .await
    .expect("reading the marker")
}

/// L-6. A cancellation request is classified against current truth: the three live states record
/// the request, the four terminal ones answer with what already happened and write nothing.
///
/// The schema comment on `cancellation_requested_at` fixes the semantics — a request, not a state —
/// so recording one must never touch `status`, `status_changed_at` or `terminated_at`.
#[tokio::test]
async fn cancellation_requests_classify_against_current_truth() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for source in transition::ALL {
        let owner_id = owner();
        let operation_id = seeded_in_status(pool, owner_id, source).await;

        let mut transaction = pool.begin().await.expect("a transaction");
        let outcome = platform_operations::request_cancellation(
            &mut transaction,
            operation_id,
            owner_id,
            now(),
        )
        .await
        .unwrap_or_else(|error| panic!("{source:?}: the classification failed: {error}"));

        if transition::is_terminal(source) {
            assert!(
                matches!(&outcome, Cancellation::Terminal(recorded) if recorded.status == source),
                "{source:?} is terminal, so the answer is current truth, got {outcome:?}"
            );
            assert!(
                !marked(&mut *transaction, operation_id).await,
                "{source:?}: answering with truth must not record a request"
            );
        } else {
            assert!(
                matches!(&outcome, Cancellation::Requested(recorded) if recorded.status == source),
                "{source:?} is live, so the request is recorded, got {outcome:?}"
            );
            assert!(
                marked(&mut *transaction, operation_id).await,
                "{source:?}: a recorded request must land in the marker column"
            );
        }
        transaction.commit().await.expect("commit");
    }

    harness.cleanup().await.expect("dropping the test database");
}

/// L-7. A repeat finds its own earlier request and writes nothing new; a foreign owner is refused
/// exactly like a missing row.
///
/// The marker is read back from the database after the first request and compared, byte for byte,
/// against what the database reports after the repeat and after the refused foreign attempt: a
/// second acceptance must not move it, because moving it would let repeated cancels extend how long
/// a downstream consumer should keep watching work that was asked to stop once. Both sides of every
/// comparison are values `PostgreSQL` itself returned, so the comparison is immune to `timestamptz`
/// storing only microsecond resolution while the in-memory `jiff::Timestamp` the caller passed in
/// carries nanoseconds — there is no local timestamp arithmetic here to drift from what the column
/// actually stores.
#[tokio::test]
async fn repeated_and_foreign_cancellation_requests_write_nothing_new() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner_id = owner();

    let operation = platform_operations::accept(
        pool,
        owner_id,
        "content.capture.submit",
        CORRELATION,
        None,
        now(),
    )
    .await
    .expect("accepting");

    let first_at = now();
    let mut transaction = pool.begin().await.expect("a transaction");
    let first = platform_operations::request_cancellation(
        &mut transaction,
        operation.operation_id,
        owner_id,
        first_at,
    )
    .await
    .expect("the first request is recorded");
    assert!(
        matches!(&first, Cancellation::Requested(_)),
        "expected a fresh request, got {first:?}"
    );
    transaction.commit().await.expect("commit");

    let original_marker: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "select cancellation_requested_at from operations.operations where operation_id = $1",
    )
    .bind(operation.operation_id)
    .fetch_one(pool)
    .await
    .expect("reading the row");

    let later = now() + jiff::SignedDuration::from_secs(300);
    let mut transaction = pool.begin().await.expect("a transaction");
    let repeat = platform_operations::request_cancellation(
        &mut transaction,
        operation.operation_id,
        owner_id,
        later,
    )
    .await
    .expect("a repeat is an answer, not an error");
    assert!(
        matches!(&repeat, Cancellation::AlreadyRequested(recorded) if recorded.status == OperationStatus::Accepted),
        "expected the earlier request to be found, got {repeat:?}"
    );
    transaction.commit().await.expect("commit");

    let marker: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "select cancellation_requested_at from operations.operations where operation_id = $1",
    )
    .bind(operation.operation_id)
    .fetch_one(pool)
    .await
    .expect("reading the row");
    assert_eq!(
        marker, original_marker,
        "the repeat must not move the original marker"
    );

    let mut transaction = pool.begin().await.expect("a transaction");
    let foreign = platform_operations::request_cancellation(
        &mut transaction,
        operation.operation_id,
        Uuid::now_v7(),
        now(),
    )
    .await;
    assert!(
        matches!(foreign, Err(OperationError::NotFound)),
        "another owner's operation must be indistinguishable from a missing one, got {foreign:?}"
    );
    transaction.rollback().await.expect("rollback");

    let untouched: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "select cancellation_requested_at from operations.operations where operation_id = $1",
    )
    .bind(operation.operation_id)
    .fetch_one(pool)
    .await
    .expect("reading the row");
    assert_eq!(
        untouched, original_marker,
        "a refused caller must leave the row as it was"
    );

    harness.cleanup().await.expect("dropping the test database");
}

/// L-8. A cancellation attempt racing a completion report or the stale-operation reaper resolves
/// to one truthful outcome, whichever transaction wins the row.
///
/// Both races are driven from two connections released by one barrier, so neither side can rely on
/// ordering. The completion always lands, the reaper still fails an operation that is truly
/// lifeless even when a stop was requested — recording `failed` with the stable staleness code
/// rather than claiming the service confirmed stopping — and the marker column agrees with what
/// its own transaction saw. Any interleaving that tripped the transition-guard trigger would error
/// one of these transactions and fail the test.
#[allow(
    clippy::too_many_lines,
    reason = "both interleavings must share one seeded database; splitting them would test two databases and prove less"
)]
#[tokio::test]
async fn cancellation_races_resolve_to_one_truthful_outcome() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner_id = owner();

    // --- Race one: cancellation against a completion report. ---
    let operation = platform_operations::accept(
        pool,
        owner_id,
        "content.capture.submit",
        CORRELATION,
        None,
        now(),
    )
    .await
    .expect("accepting");

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let canceller = {
        let barrier = Arc::clone(&barrier);
        let pool = pool.clone();
        let operation_id = operation.operation_id;
        tokio::spawn(async move {
            barrier.wait().await;
            let mut transaction = pool.begin().await.expect("a transaction");
            let outcome = platform_operations::request_cancellation(
                &mut transaction,
                operation_id,
                owner_id,
                now(),
            )
            .await
            .expect("cancellation is never an error");
            transaction.commit().await.expect("commit");
            outcome
        })
    };
    let completer = {
        let barrier = Arc::clone(&barrier);
        let pool = pool.clone();
        let operation_id = operation.operation_id;
        tokio::spawn(async move {
            barrier.wait().await;
            let mut transaction = pool.begin().await.expect("a transaction");
            let (outcome, _) = platform_operations::record_status(
                &mut transaction,
                operation_id,
                OperationStatus::Succeeded,
                None,
                None,
                None,
                now(),
            )
            .await
            .expect("the completion must land");
            transaction.commit().await.expect("commit");
            outcome
        })
    };

    let cancellation = canceller.await.expect("no panic");
    let completion = completer.await.expect("no panic");
    assert_eq!(
        completion,
        Transition::Advance(OperationStatus::Succeeded),
        "accepted -> succeeded is a legal skip advance regardless of who went first"
    );

    let (final_status, terminated, marked_now): (String, Option<time::OffsetDateTime>, bool) =
        sqlx::query_as(
            "select status, terminated_at, cancellation_requested_at is not null
               from operations.operations where operation_id = $1",
        )
        .bind(operation.operation_id)
        .fetch_one(pool)
        .await
        .expect("reading the row");
    assert_eq!(final_status, "succeeded", "exactly one terminal winner");
    assert!(
        terminated.is_some(),
        "the terminal state carries its instant"
    );
    assert_eq!(
        matches!(&cancellation, Cancellation::Requested(_)),
        marked_now,
        "the marker records exactly what the cancelling transaction observed"
    );
    assert!(
        !matches!(&cancellation, Cancellation::Terminal(recorded) if recorded.status != OperationStatus::Succeeded),
        "terminal truth names the real terminal status"
    );

    // --- Race two: cancellation against the reaper on a genuinely stale operation. ---
    let stale_id = Uuid::now_v7();
    sqlx::query(
        "insert into operations.operations
             (operation_id, owner_user_id, kind, status, correlation_id,
              accepted_at, status_changed_at)
         values ($1, $2, 'content.capture.submit', 'accepted', $3,
                 now() - interval '48 hours', now() - interval '48 hours')",
    )
    .bind(stale_id)
    .bind(owner_id)
    .bind(CORRELATION)
    .execute(pool)
    .await
    .expect("seeding the stale operation");

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let canceller = {
        let barrier = Arc::clone(&barrier);
        let pool = pool.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let mut transaction = pool.begin().await.expect("a transaction");
            let outcome = platform_operations::request_cancellation(
                &mut transaction,
                stale_id,
                owner_id,
                now(),
            )
            .await
            .expect("cancellation is never an error");
            transaction.commit().await.expect("commit");
            outcome
        })
    };
    let reaper = {
        let barrier = Arc::clone(&barrier);
        let pool = pool.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            platform_operations::reconcile::run_once(
                &pool,
                jiff::SignedDuration::from_hours(24),
                10,
                now(),
            )
            .await
            .expect("the pass completes")
        })
    };

    let cancellation = canceller.await.expect("no panic");
    let report = reaper.await.expect("no panic");
    assert_eq!(
        report.reconciled, 1,
        "a pending stop request is not a sign of life"
    );

    let (final_status, marked_later, retryable): (String, bool, bool) = sqlx::query_as(
        "select status, cancellation_requested_at is not null, retryable
           from operations.operations where operation_id = $1",
    )
    .bind(stale_id)
    .fetch_one(pool)
    .await
    .expect("reading the row");
    assert_eq!(
        final_status, "failed",
        "silence is failed with the staleness code, never cancelled by Platform"
    );
    assert!(retryable, "the staleness fault is resubmittable");
    assert_eq!(
        matches!(&cancellation, Cancellation::Requested(_)),
        marked_later,
        "the marker again agrees with its own transaction's view"
    );

    let stale_code: String = sqlx::query_scalar(
        "select code from operations.operation_errors
          where operation_id = $1 and severity = 'error'",
    )
    .bind(stale_id)
    .fetch_one(pool)
    .await
    .expect("the staleness error is recorded");
    assert_eq!(
        stale_code,
        platform_operations::reconcile::STALE_ERROR_CODE,
        "the stable code survives the race"
    );

    harness.cleanup().await.expect("dropping the test database");
}

/// The listing fixture: four rows for the owner across kinds and statuses, one for the stranger.
///
/// Acceptance instants are pinned explicitly, because the page order is `accepted_at` descending:
/// `oldest` was accepted 40 minutes before `base`, then running at 30, succeeded at 20 and
/// `social` at 10 — so newest-first reads [social, succeeded, running, oldest].
struct ListingFixture {
    oldest: platform_operations::Operation,
    running: platform_operations::Operation,
    succeeded: platform_operations::Operation,
    social: platform_operations::Operation,
    strangers_row: platform_operations::Operation,
}

const CONTENT_KIND: &str = "content.capture.submit";
const SOCIAL_KIND: &str = "social.source.sync";

async fn listing_fixture(pool: &sqlx::PgPool, owner_id: Uuid, stranger: Uuid) -> ListingFixture {
    let base = now() - jiff::SignedDuration::from_hours(1);
    let at =
        |minutes_before_base: i64| base - jiff::SignedDuration::from_secs(60 * minutes_before_base);
    let oldest =
        platform_operations::accept(pool, owner_id, CONTENT_KIND, CORRELATION, None, at(40))
            .await
            .expect("accepting a fixture row");
    let running =
        platform_operations::accept(pool, owner_id, CONTENT_KIND, CORRELATION, None, at(30))
            .await
            .expect("accepting a fixture row");
    let succeeded =
        platform_operations::accept(pool, owner_id, SOCIAL_KIND, CORRELATION, None, at(20))
            .await
            .expect("accepting a fixture row");
    let social =
        platform_operations::accept(pool, owner_id, SOCIAL_KIND, CORRELATION, None, at(10))
            .await
            .expect("accepting a fixture row");
    let strangers_row =
        platform_operations::accept(pool, stranger, CONTENT_KIND, CORRELATION, None, at(25))
            .await
            .expect("accepting a fixture row");

    for (row, status) in [
        (&running, OperationStatus::Running),
        (&succeeded, OperationStatus::Succeeded),
    ] {
        let mut transaction = pool.begin().await.expect("a transaction");
        platform_operations::record_status(
            &mut transaction,
            row.operation_id,
            status,
            None,
            None,
            None,
            now(),
        )
        .await
        .expect("advancing a fixture row");
        transaction.commit().await.expect("commit");
    }

    ListingFixture {
        oldest,
        running,
        succeeded,
        social,
        strangers_row,
    }
}

fn scope(
    owner_id: Uuid,
    status: Option<OperationStatus>,
    kind: Option<&str>,
    before: Option<(jiff::Timestamp, Uuid)>,
    limit: i64,
) -> platform_operations::ListScope<'_> {
    platform_operations::ListScope {
        owner_user_id: owner_id,
        status,
        kind,
        before,
        limit,
    }
}

fn ids(page: &platform_operations::Page) -> Vec<Uuid> {
    page.rows.iter().map(|row| row.operation_id).collect()
}

/// L-9a. The unfiltered listing answers newest accepted first and never crosses owners.
#[tokio::test]
async fn the_listing_orders_newest_first_and_stays_tenant_scoped() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner_id = owner();
    let stranger = owner();
    let fixture = listing_fixture(pool, owner_id, stranger).await;

    let page = platform_operations::list_operations(pool, scope(owner_id, None, None, None, 10))
        .await
        .expect("listing");
    assert_eq!(
        ids(&page),
        vec![
            fixture.social.operation_id,
            fixture.succeeded.operation_id,
            fixture.running.operation_id,
            fixture.oldest.operation_id,
        ]
    );
    assert!(!page.has_more);

    let foreign = platform_operations::list_operations(pool, scope(stranger, None, None, None, 10))
        .await
        .expect("stranger listing");
    assert_eq!(ids(&foreign), vec![fixture.strangers_row.operation_id]);

    harness.cleanup().await.expect("dropping the test database");
}

/// L-9b. Status and kind filters bind exactly and combine by conjunction.
#[tokio::test]
async fn listing_filters_combine_by_conjunction() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner_id = owner();
    let fixture = listing_fixture(pool, owner_id, owner()).await;

    let running = platform_operations::list_operations(
        pool,
        scope(owner_id, Some(OperationStatus::Running), None, None, 10),
    )
    .await
    .expect("listing");
    assert_eq!(ids(&running), vec![fixture.running.operation_id]);

    // Both social-kind rows, newest accepted first: `social` at -10 beats `succeeded` at -20.
    let social = platform_operations::list_operations(
        pool,
        scope(owner_id, None, Some(SOCIAL_KIND), None, 10),
    )
    .await
    .expect("listing");
    assert_eq!(
        ids(&social),
        vec![fixture.social.operation_id, fixture.succeeded.operation_id]
    );

    let both = platform_operations::list_operations(
        pool,
        scope(
            owner_id,
            Some(OperationStatus::Succeeded),
            Some(SOCIAL_KIND),
            None,
            10,
        ),
    )
    .await
    .expect("listing");
    assert_eq!(ids(&both), vec![fixture.succeeded.operation_id]);

    let neither = platform_operations::list_operations(
        pool,
        scope(
            owner_id,
            Some(OperationStatus::Failed),
            Some(CONTENT_KIND),
            None,
            10,
        ),
    )
    .await
    .expect("listing");
    assert!(neither.rows.is_empty() && !neither.has_more);

    harness.cleanup().await.expect("dropping the test database");
}

/// L-9c. Pages walk a keyset cursor: nothing repeats, none is lost, and an operation accepted
/// after a page was served belongs to that already-served page rather than shifting later ones.
#[tokio::test]
async fn pages_walk_a_keyset_cursor_without_drift() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner_id = owner();
    let fixture = listing_fixture(pool, owner_id, owner()).await;

    let first = platform_operations::list_operations(pool, scope(owner_id, None, None, None, 2))
        .await
        .expect("first page");
    assert_eq!(
        ids(&first),
        vec![fixture.social.operation_id, fixture.succeeded.operation_id],
        "the newest two, in order"
    );
    assert!(first.has_more);

    // Newer than everything the walk has seen: it lives on the taken page's territory, so the
    // continuation must still be exactly the older half.
    let newcomer = platform_operations::accept(
        pool,
        owner_id,
        CONTENT_KIND,
        CORRELATION,
        None,
        now() + jiff::SignedDuration::from_hours(1),
    )
    .await
    .expect("accepting the latecomer");

    let anchor = &fixture.succeeded;
    let second = platform_operations::list_operations(
        pool,
        scope(
            owner_id,
            None,
            None,
            Some((anchor.accepted_at, anchor.operation_id)),
            2,
        ),
    )
    .await
    .expect("second page");
    assert_eq!(
        ids(&second),
        vec![fixture.running.operation_id, fixture.oldest.operation_id]
    );
    assert!(!second.has_more);

    let fresh = platform_operations::list_operations(pool, scope(owner_id, None, None, None, 2))
        .await
        .expect("fresh page");
    assert_eq!(
        ids(&fresh),
        vec![newcomer.operation_id, fixture.social.operation_id]
    );

    harness.cleanup().await.expect("dropping the test database");
}
