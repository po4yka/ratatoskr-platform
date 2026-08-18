//! The outbox and inbox, against a real `PostgreSQL`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_eventing::inbox::Outcome;
use platform_eventing::{Inbox, MessageClass, Outbox, Reception, Subject};
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn command() -> Subject {
    Subject::new(MessageClass::Command, "content.capture.requested.v1").expect("a valid subject")
}

fn payload() -> serde_json::Value {
    serde_json::json!({"event_id": "01a0153f-63e5-7010-a4c9-1fe6c43bcc39"})
}

/// O-1. The outbox write joins the caller's transaction. If the transaction rolls back, no message
/// exists — which is the entire reason the outbox is a table and not a queue client.
#[tokio::test]
async fn a_rolled_back_transaction_leaves_no_message() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let mut transaction = pool.begin().await.expect("a transaction");
    Outbox::enqueue(
        &mut *transaction,
        Uuid::now_v7(),
        &command(),
        &payload(),
        None,
        now(),
    )
    .await
    .expect("enqueuing");
    transaction.rollback().await.expect("rollback");

    let stats = Outbox::stats(pool, now()).await.expect("stats");
    assert_eq!(
        stats.pending, 0,
        "a rolled back transaction must publish nothing"
    );

    harness.cleanup().await.expect("cleanup");
}

/// O-2. Enqueuing is idempotent on the message id, so a retried request cannot produce two messages.
#[tokio::test]
async fn enqueuing_the_same_message_twice_produces_one_row() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let message_id = Uuid::now_v7();

    assert!(
        Outbox::enqueue(pool, message_id, &command(), &payload(), None, now())
            .await
            .expect("the first")
    );
    assert!(
        !Outbox::enqueue(pool, message_id, &command(), &payload(), None, now())
            .await
            .expect("the second"),
        "a repeated message id must not enqueue a second row"
    );
    assert_eq!(Outbox::stats(pool, now()).await.expect("stats").pending, 1);

    harness.cleanup().await.expect("cleanup");
}

/// O-3. Two publishers claiming at once take disjoint sets and neither blocks.
#[tokio::test]
async fn two_publishers_never_claim_the_same_message() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for _ in 0..10 {
        Outbox::enqueue(pool, Uuid::now_v7(), &command(), &payload(), None, now())
            .await
            .expect("enqueuing");
    }

    let (first, second) = tokio::join!(
        Outbox::claim(pool, "publisher-a", 10, now()),
        Outbox::claim(pool, "publisher-b", 10, now()),
    );
    let first = first.expect("claim a");
    let second = second.expect("claim b");

    assert_eq!(
        first.len() + second.len(),
        10,
        "every message must be claimed once"
    );
    for a in &first {
        assert!(
            !second.iter().any(|b| b.outbox_id == a.outbox_id),
            "the same row was claimed twice"
        );
    }

    // A third publisher finds nothing: everything is leased.
    assert!(
        Outbox::claim(pool, "publisher-c", 10, now())
            .await
            .expect("claim c")
            .is_empty()
    );

    harness.cleanup().await.expect("cleanup");
}

/// O-4. A claim is a lease. A publisher that dies mid-batch does not strand its messages.
#[tokio::test]
async fn an_expired_claim_returns_the_message_to_the_queue() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    Outbox::enqueue(pool, Uuid::now_v7(), &command(), &payload(), None, now())
        .await
        .expect("enqueuing");

    let claimed = Outbox::claim(pool, "publisher-that-dies", 10, now())
        .await
        .expect("claiming");
    assert_eq!(claimed.len(), 1);

    let expiry = Outbox::claim_expiry(pool, claimed[0].outbox_id)
        .await
        .expect("reading the lease")
        .expect("a lease exists");

    // Nothing is claimable while the lease holds.
    assert!(
        Outbox::claim(pool, "another", 10, now())
            .await
            .expect("claiming")
            .is_empty()
    );

    // After it expires, the row is claimable again without anyone releasing it.
    let after = expiry + jiff::SignedDuration::from_secs(1);
    let reclaimed = Outbox::claim(pool, "another", 10, after)
        .await
        .expect("reclaiming");
    assert_eq!(
        reclaimed.len(),
        1,
        "an expired lease must return the message"
    );

    harness.cleanup().await.expect("cleanup");
}

/// O-5. Failure backs the message off, and exhaustion dead-letters it rather than dropping it or
/// retrying forever.
#[tokio::test]
async fn an_exhausted_message_is_dead_lettered_not_dropped() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    Outbox::enqueue(pool, Uuid::now_v7(), &command(), &payload(), None, now())
        .await
        .expect("enqueuing");
    let claimed = Outbox::claim(pool, "publisher", 1, now())
        .await
        .expect("claiming");
    let outbox_id = claimed[0].outbox_id;

    // A multi-line error must not break the column's rule.
    let dead = Outbox::mark_failed(
        pool,
        outbox_id,
        "connection refused\n  caused by: broken pipe",
        now(),
    )
    .await
    .expect("failing");
    assert!(!dead, "one failure must not dead-letter");

    // Backed off: not claimable at the same instant.
    assert!(
        Outbox::claim(pool, "publisher", 1, now())
            .await
            .expect("claiming")
            .is_empty(),
        "a backed-off message must not be immediately reclaimable"
    );

    // Drive it to exhaustion. The attempt ceiling is an implementation constant; failing far past it
    // proves the ceiling exists rather than asserting its value.
    let mut dead_lettered = false;
    for attempt in 0..20 {
        let future = now() + jiff::SignedDuration::from_secs(3600 * (attempt + 1));
        let claimed = Outbox::claim(pool, "publisher", 1, future)
            .await
            .expect("claiming");
        if claimed.is_empty() {
            break;
        }
        dead_lettered = Outbox::mark_failed(pool, outbox_id, "still refused", future)
            .await
            .expect("failing");
        if dead_lettered {
            break;
        }
    }
    assert!(dead_lettered, "an exhausted message must be dead-lettered");

    let stats = Outbox::stats(pool, now()).await.expect("stats");
    assert_eq!(stats.dead_lettered, 1);
    assert_eq!(
        stats.pending, 0,
        "a dead-lettered message is no longer pending"
    );

    // It is still there, with its diagnosis. AGENTS.md requires a diagnosable path, not a drop.
    let last_error: String =
        sqlx::query_scalar("select last_error from operations.outbox where outbox_id = $1")
            .bind(outbox_id)
            .fetch_one(pool)
            .await
            .expect("reading");
    assert!(!last_error.is_empty());

    harness.cleanup().await.expect("cleanup");
}

/// O-6. Publishing clears the claim and stops the row being pending.
#[tokio::test]
async fn a_published_message_leaves_the_queue() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    Outbox::enqueue(pool, Uuid::now_v7(), &command(), &payload(), None, now())
        .await
        .expect("enqueuing");
    let claimed = Outbox::claim(pool, "publisher", 1, now())
        .await
        .expect("claiming");
    Outbox::mark_published(pool, claimed[0].outbox_id, now())
        .await
        .expect("publishing");

    let stats = Outbox::stats(pool, now()).await.expect("stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.oldest_pending_age_seconds, 0);
    assert!(
        Outbox::claim(pool, "publisher", 1, now())
            .await
            .expect("claiming")
            .is_empty(),
        "a published message must never be claimed again"
    );

    harness.cleanup().await.expect("cleanup");
}

/// O-7. The inbox deduplicates in one statement, so two workers cannot both believe they are first.
#[tokio::test]
async fn the_inbox_admits_a_message_once() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let message_id = Uuid::now_v7();
    let subject = Subject::new(MessageClass::Event, "platform.operation.progressed.v1")
        .expect("a valid subject");

    let (a, b) = tokio::join!(
        Inbox::begin(pool, message_id, &subject, "ratatoskr-platform", now()),
        Inbox::begin(pool, message_id, &subject, "ratatoskr-platform", now()),
    );
    let receptions = [a.expect("a"), b.expect("b")];
    assert_eq!(
        receptions
            .iter()
            .filter(|r| **r == Reception::First)
            .count(),
        1,
        "exactly one caller may be first"
    );
    assert_eq!(
        receptions
            .iter()
            .filter(|r| **r == Reception::Duplicate)
            .count(),
        1
    );

    assert_eq!(Inbox::unprocessed(pool).await.expect("counting"), 1);
    Inbox::finish(pool, message_id, Outcome::Applied, now())
        .await
        .expect("finishing");
    assert_eq!(
        Inbox::unprocessed(pool).await.expect("counting"),
        0,
        "a finished message is no longer outstanding"
    );

    harness.cleanup().await.expect("cleanup");
}

/// O-8. The lag signal distinguishes a busy queue from a stuck one.
#[tokio::test]
async fn the_lag_signal_reports_the_age_of_the_oldest_pending_message() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let old = now() - jiff::SignedDuration::from_secs(120);
    Outbox::enqueue(pool, Uuid::now_v7(), &command(), &payload(), None, old)
        .await
        .expect("enqueuing an old message");
    Outbox::enqueue(pool, Uuid::now_v7(), &command(), &payload(), None, now())
        .await
        .expect("enqueuing a new one");

    let stats = Outbox::stats(pool, now()).await.expect("stats");
    assert_eq!(stats.pending, 2);
    assert!(
        stats.oldest_pending_age_seconds >= 119,
        "the lag must reflect the OLDEST message, got {}",
        stats.oldest_pending_age_seconds
    );

    harness.cleanup().await.expect("cleanup");
}
