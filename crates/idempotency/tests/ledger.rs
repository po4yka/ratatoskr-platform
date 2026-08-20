//! The idempotency ledger, against a real `PostgreSQL`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_idempotency::{Digest, Reservation, complete, reserve};
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

const ROUTE: &str = "/v1/captures";
const KIND: &str = "content.capture.submit";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn ttl() -> jiff::SignedDuration {
    jiff::SignedDuration::from_hours(24)
}

/// K-1. A new key reserves; the same key with the same payload replays the first answer instead of
/// doing the work twice.
#[tokio::test]
async fn the_same_key_and_payload_replays_the_original_answer() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = Uuid::now_v7();
    let key = Digest::of_key("client-chosen-key");
    let body = Digest::of_body(br#"{"url":"https://example.test/a"}"#);

    // A real operation: the ledger references one, and a foreign key means an invented id is
    // rejected. This crate does not depend on `ratatoskr-platform-operations`, so the row is written
    // directly rather than pulling in a dependency for one insert.
    let operation_id = Uuid::now_v7();
    sqlx::query(
        "insert into operations.operations
             (operation_id, owner_user_id, kind, status, correlation_id, accepted_at, status_changed_at)
         values ($1, $2, $3, 'accepted', $4, now(), now())",
    )
    .bind(operation_id)
    .bind(owner)
    .bind(KIND)
    .bind("correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39")
    .execute(pool)
    .await
    .expect("seeding an operation");

    let mut transaction = pool.begin().await.expect("a transaction");
    let first = reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        key,
        body,
        now(),
        ttl(),
    )
    .await
    .expect("reserving");
    let Reservation::Fresh { record_id } = first else {
        panic!("the first reservation must be fresh, got {first:?}");
    };
    complete(&mut *transaction, record_id, Some(operation_id), 202, now())
        .await
        .expect("completing");
    transaction.commit().await.expect("commit");

    let mut transaction = pool.begin().await.expect("a transaction");
    let second = reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        key,
        body,
        now(),
        ttl(),
    )
    .await
    .expect("reserving again");
    transaction.commit().await.expect("commit");

    assert_eq!(
        second,
        Reservation::Replay {
            operation_id: Some(operation_id),
            response_status: 202,
        },
        "a retry must return the original operation, not a new one"
    );

    harness.cleanup().await.expect("cleanup");
}

/// K-2. The same key with a different payload is rejected. Honouring it would let a client silently
/// replace the meaning of a request it already sent.
#[tokio::test]
async fn the_same_key_with_a_different_payload_conflicts() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = Uuid::now_v7();
    let key = Digest::of_key("k");

    let mut transaction = pool.begin().await.expect("a transaction");
    let Reservation::Fresh { record_id } = reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        key,
        Digest::of_body(b"first"),
        now(),
        ttl(),
    )
    .await
    .expect("reserving") else {
        panic!("expected a fresh reservation");
    };
    complete(&mut *transaction, record_id, None, 202, now())
        .await
        .expect("completing");
    transaction.commit().await.expect("commit");

    let mut transaction = pool.begin().await.expect("a transaction");
    let conflict = reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        key,
        Digest::of_body(b"second"),
        now(),
        ttl(),
    )
    .await
    .expect("reserving");
    transaction.commit().await.expect("commit");

    assert_eq!(conflict, Reservation::Conflict);

    harness.cleanup().await.expect("cleanup");
}

/// K-3. A retry that arrives while the first attempt is still running is told so, rather than being
/// allowed to start a second operation.
#[tokio::test]
async fn a_retry_during_the_first_attempt_is_in_flight() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = Uuid::now_v7();
    let key = Digest::of_key("k");
    let body = Digest::of_body(b"payload");

    let mut first = pool.begin().await.expect("a transaction");
    reserve(&mut first, owner, ROUTE, KIND, key, body, now(), ttl())
        .await
        .expect("reserving");
    first.commit().await.expect("commit without completing");

    let mut second = pool.begin().await.expect("a transaction");
    let outcome = reserve(&mut second, owner, ROUTE, KIND, key, body, now(), ttl())
        .await
        .expect("reserving");
    second.commit().await.expect("commit");

    assert_eq!(outcome, Reservation::InFlight);

    harness.cleanup().await.expect("cleanup");
}

/// K-4. The scope is actor, route and kind. The same key is free in every other scope.
#[tokio::test]
async fn a_key_is_only_taken_inside_its_own_scope() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let key = Digest::of_key("shared");
    let body = Digest::of_body(b"payload");
    let owner = Uuid::now_v7();

    let mut transaction = pool.begin().await.expect("a transaction");
    reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        key,
        body,
        now(),
        ttl(),
    )
    .await
    .expect("the original");

    for (other_owner, route, kind) in [
        (Uuid::now_v7(), ROUTE, KIND),
        (owner, "/v1/imports", KIND),
        (owner, ROUTE, "social.source.sync"),
    ] {
        let outcome = reserve(
            &mut transaction,
            other_owner,
            route,
            kind,
            key,
            body,
            now(),
            ttl(),
        )
        .await
        .expect("reserving in another scope");
        assert!(
            matches!(outcome, Reservation::Fresh { .. }),
            "the key must be free in a different scope, got {outcome:?}"
        );
    }
    transaction.commit().await.expect("commit");

    harness.cleanup().await.expect("cleanup");
}

/// K-5. The window closes. An expired reservation neither replays nor blocks, and collection removes
/// it — `DATA_MODEL.md` gives the idempotency window its own retention class.
#[tokio::test]
async fn an_expired_reservation_frees_the_key() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = Uuid::now_v7();
    let key = Digest::of_key("k");
    let body = Digest::of_body(b"payload");
    let short = jiff::SignedDuration::from_secs(60);

    let mut transaction = pool.begin().await.expect("a transaction");
    let Reservation::Fresh { record_id } = reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        key,
        body,
        now(),
        short,
    )
    .await
    .expect("reserving") else {
        panic!("expected a fresh reservation");
    };
    complete(&mut *transaction, record_id, None, 202, now())
        .await
        .expect("completing");
    transaction.commit().await.expect("commit");

    let later = now() + jiff::SignedDuration::from_secs(120);

    let mut transaction = pool.begin().await.expect("a transaction");
    let after_expiry = reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        key,
        body,
        later,
        ttl(),
    )
    .await
    .expect("reserving after expiry");
    transaction.commit().await.expect("commit");
    assert!(
        matches!(after_expiry, Reservation::Fresh { .. }),
        "an expired window must free the key, got {after_expiry:?}"
    );

    let collected = platform_idempotency::collect_expired(pool, later)
        .await
        .expect("collecting");
    assert_eq!(collected, 0, "the live reservation must survive collection");

    let much_later = later + jiff::SignedDuration::from_hours(48);
    assert_eq!(
        platform_idempotency::collect_expired(pool, much_later)
            .await
            .expect("collecting"),
        1
    );

    harness.cleanup().await.expect("cleanup");
}

/// K-6. Neither the key nor the body is recoverable from the ledger.
#[tokio::test]
async fn the_ledger_stores_no_recoverable_key_or_body() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let owner = Uuid::now_v7();

    let mut transaction = pool.begin().await.expect("a transaction");
    reserve(
        &mut transaction,
        owner,
        ROUTE,
        KIND,
        Digest::of_key("order-12345-for-alice"),
        Digest::of_body(br#"{"url":"https://private.example/secret-doc"}"#),
        now(),
        ttl(),
    )
    .await
    .expect("reserving");
    transaction.commit().await.expect("commit");

    let dumped: String = sqlx::query_scalar(
        "select coalesce(string_agg(record_id::text || route || operation_kind ||
                                    encode(key_hash,'hex') || encode(request_fingerprint,'hex'), ' '), '')
           from operations.idempotency_records",
    )
    .fetch_one(pool)
    .await
    .expect("dumping the ledger");

    for secret in ["alice", "order-12345", "private.example", "secret-doc"] {
        assert!(
            !dumped.contains(secret),
            "the ledger must not contain {secret}"
        );
    }

    // And the digest type refuses to render itself.
    assert_eq!(
        format!("{:?}", Digest::of_key("order-12345-for-alice")),
        "Digest([REDACTED])"
    );

    harness.cleanup().await.expect("cleanup");
}
