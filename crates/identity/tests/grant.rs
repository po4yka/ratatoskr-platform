//! Capability grants — tests G-1 … G-4.
//!
//! `identity.grants` was built at milestone 2 and read by nothing until milestone 8 needed to say
//! who may claim an OAuth relay. `ARCHITECTURE.md` S7 makes authorization a capability question, so
//! liveness has three parts and all three are enforced in one statement rather than by a caller who
//! has to remember them.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_identity::grant;
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

const CAPABILITY: &str = "oauth.claim.github";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

async fn a_user(pool: &sqlx::PgPool) -> Uuid {
    platform_identity::user::create_user(pool, now())
        .await
        .expect("a user")
        .user_id
}

/// G-1. A capability nobody granted is not held, and granting it makes it held.
#[tokio::test]
async fn a_granted_capability_is_held_and_an_ungranted_one_is_not() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = a_user(pool).await;

    assert!(
        !grant::holds(pool, user, CAPABILITY, now())
            .await
            .expect("a read")
    );
    grant::grant(pool, user, CAPABILITY, now(), None)
        .await
        .expect("a grant");
    assert!(
        grant::holds(pool, user, CAPABILITY, now())
            .await
            .expect("a read")
    );

    // And it is that user's, not everybody's.
    let other = a_user(pool).await;
    assert!(
        !grant::holds(pool, other, CAPABILITY, now())
            .await
            .expect("a read")
    );
}

/// G-2. Granting twice is one grant. The partial unique index is what enforces it, so a retried
/// provisioning step cannot leave two rows that revoking one of them fails to withdraw.
#[tokio::test]
async fn granting_twice_is_one_grant() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = a_user(pool).await;

    let first = grant::grant(pool, user, CAPABILITY, now(), None)
        .await
        .expect("a grant");
    let second = grant::grant(pool, user, CAPABILITY, now(), None)
        .await
        .expect("a grant");

    assert_eq!(first, second, "the second grant is the first");
    let rows: i64 = sqlx::query_scalar("select count(*) from identity.grants where user_id = $1")
        .bind(user)
        .fetch_one(pool)
        .await
        .expect("a count");
    assert_eq!(rows, 1);
}

/// G-3. Revoking withdraws it, and the row stays so an audit reader can say when it stopped.
#[tokio::test]
async fn revoking_withdraws_it_and_keeps_the_record() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = a_user(pool).await;
    grant::grant(pool, user, CAPABILITY, now(), None)
        .await
        .expect("a grant");

    assert!(
        grant::revoke(pool, user, CAPABILITY, now())
            .await
            .expect("a revocation")
    );
    assert!(
        !grant::holds(pool, user, CAPABILITY, now())
            .await
            .expect("a read")
    );
    assert!(
        !grant::revoke(pool, user, CAPABILITY, now())
            .await
            .expect("a second revocation"),
        "there was nothing live left to withdraw"
    );

    let revoked: i64 = sqlx::query_scalar(
        "select count(*) from identity.grants where user_id = $1 and revoked_at is not null",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("a count");
    assert_eq!(revoked, 1, "the row survives its revocation");

    // And a fresh grant is possible afterwards: the unique index is partial on live rows.
    grant::grant(pool, user, CAPABILITY, now(), None)
        .await
        .expect("a second grant");
    assert!(
        grant::holds(pool, user, CAPABILITY, now())
            .await
            .expect("a read")
    );
}

/// G-4. An expired grant is not held, without anything having to sweep it.
#[tokio::test]
async fn an_expired_grant_is_not_held() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = a_user(pool).await;
    let granted_at = now() - jiff::SignedDuration::from_hours(2);
    let expires_at = now() - jiff::SignedDuration::from_hours(1);
    grant::grant(pool, user, CAPABILITY, granted_at, Some(expires_at))
        .await
        .expect("a grant");

    assert!(
        !grant::holds(pool, user, CAPABILITY, now())
            .await
            .expect("a read")
    );
    assert!(
        grant::holds(
            pool,
            user,
            CAPABILITY,
            expires_at - jiff::SignedDuration::from_mins(1)
        )
        .await
        .expect("a read"),
        "and it was held while it lasted"
    );
}
