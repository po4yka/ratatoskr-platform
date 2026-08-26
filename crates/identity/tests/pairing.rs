//! Pairing codes, against a real `PostgreSQL`: single use, expiry, supersession and kind binding.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_identity::pairing::{NewPairingCode, PairRefused, PairRequest};
use platform_identity::{DeviceKind, NewSession, SecretDigest, SessionKind};
use platform_persistence::test_support::TestDatabase;

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn later(minutes: i64) -> jiff::Timestamp {
    now() + jiff::SignedDuration::from_mins(minutes)
}

fn digest(seed: u8) -> SecretDigest {
    SecretDigest::new([seed; 32])
}

/// A user with one live browser session: the trusted context pairing starts from.
async fn a_trusted_context(harness: &TestDatabase) -> (uuid::Uuid, uuid::Uuid) {
    let pool = harness.pool();
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let session = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind: SessionKind::Browser,
            device_id: None,
            audience: "edge",
            token: None,
            issued_at: now(),
            expires_at: later(60),
        },
    )
    .await
    .expect("a session");
    (user.user_id, session.session_id)
}

fn new_code(
    user_id: uuid::Uuid,
    created_by_session_id: uuid::Uuid,
    code_digest: SecretDigest,
    expected_kind: Option<DeviceKind>,
    minted_at: jiff::Timestamp,
) -> NewPairingCode<'static> {
    NewPairingCode {
        user_id,
        created_by_session_id,
        expected_kind,
        label: Some("pixel phone"),
        code_digest,
        now: minted_at,
        expires_at: minted_at + jiff::SignedDuration::from_mins(10),
    }
}

fn pair_request(
    presented: SecretDigest,
    declared_kind: DeviceKind,
    access: u8,
) -> PairRequest<'static> {
    PairRequest {
        presented,
        declared_kind,
        display_name: Some("the pixel"),
        device_secret: digest(access),
        access_token: digest(access.wrapping_add(1)),
        refresh_token: digest(access.wrapping_add(2)),
        audience: "edge",
        now: now(),
        access_expires_at: later(60),
        refresh_expires_at: later(60 * 24 * 30),
    }
}

/// P-1. A live code grants exactly once: a device under the OWNER, a bound live session, a first
/// refresh link — and never again.
#[tokio::test]
async fn a_live_code_redeems_once_into_device_session_and_refresh() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, creator_session) = a_trusted_context(&harness).await;

    let mut transaction = pool.begin().await.expect("a transaction");
    let created = platform_identity::pairing::create_code(
        &mut transaction,
        &new_code(
            owner,
            creator_session,
            digest(1),
            Some(DeviceKind::Mobile),
            now(),
        ),
    )
    .await
    .expect("a pairing code");
    transaction.commit().await.expect("commit");
    assert_eq!(created.user_id, owner);
    assert_eq!(created.expected_kind, Some(DeviceKind::Mobile));
    assert!(created.superseded_at.is_none() && created.consumed_at.is_none());

    let mut transaction = pool.begin().await.expect("a transaction");
    let redeemed = platform_identity::pairing::redeem(
        &mut transaction,
        &pair_request(digest(1), DeviceKind::Mobile, 100),
    )
    .await
    .expect("redeem runs")
    .expect("a live code redeems");
    transaction.commit().await.expect("commit");

    assert_eq!(redeemed.user_id, owner, "the grant binds to the owner");
    assert_eq!(redeemed.device.user_id, owner);
    assert_eq!(redeemed.device.kind, DeviceKind::Mobile);
    assert!(redeemed.device.is_active());
    assert_eq!(redeemed.session.kind, SessionKind::Device);
    assert_eq!(redeemed.session.device_id, Some(redeemed.device.device_id));
    assert!(redeemed.session.is_live_at(now()));

    // Single-use is observable state, not a promise.
    let consumed = platform_identity::pairing::find_code(pool, digest(1))
        .await
        .expect("reading the code back")
        .expect("it exists");
    assert!(consumed.consumed_at.is_some());
    assert_eq!(
        consumed.consumed_by_device_id,
        Some(redeemed.device.device_id)
    );

    // The second presentation of the same code is refused like every other refusal.
    let mut transaction = pool.begin().await.expect("a transaction");
    let again = platform_identity::pairing::redeem(
        &mut transaction,
        &pair_request(digest(1), DeviceKind::Mobile, 200),
    )
    .await
    .expect("redeem runs");
    transaction.commit().await.expect("commit");
    assert_eq!(again, Err(PairRefused));

    harness.cleanup().await.expect("cleanup");
}

/// P-2. Expired, superseded and kind-mismatched codes all refuse, leaving no grants behind.
#[tokio::test]
async fn expired_superseded_and_kind_mismatched_codes_refuse() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, creator_session) = a_trusted_context(&harness).await;

    // An expired code: minted in the past, its expiry already behind us.
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &new_code(
            owner,
            creator_session,
            digest(10),
            None,
            now() - jiff::SignedDuration::from_mins(11),
        ),
    )
    .await
    .expect("a code whose expiry has passed");
    transaction.commit().await.expect("commit");

    // A fresh code for the same owner must set the abandoned one aside — including this expired
    // one, which is still marked pending. Supersession, not a sweep, keeps the flow un-wedged.
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &new_code(
            owner,
            creator_session,
            digest(11),
            Some(DeviceKind::Mobile),
            now(),
        ),
    )
    .await
    .expect("a fresh code");
    transaction.commit().await.expect("commit");
    let old = platform_identity::pairing::find_code(pool, digest(10))
        .await
        .expect("reading back")
        .expect("the abandoned code still exists as history");
    assert!(
        old.superseded_at.is_some(),
        "creating a new code supersedes the previous pending one"
    );

    // A kind mismatch refuses WITHOUT consuming the pinned code.
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &new_code(
            owner,
            creator_session,
            digest(12),
            Some(DeviceKind::ExportAgent),
            now(),
        ),
    )
    .await
    .expect("a pinned code");
    transaction.commit().await.expect("commit");

    let mut transaction = pool.begin().await.expect("a transaction");
    let wrong_kind = platform_identity::pairing::redeem(
        &mut transaction,
        &pair_request(digest(12), DeviceKind::Mobile, 100),
    )
    .await
    .expect("redeem runs");
    transaction.commit().await.expect("commit");
    assert_eq!(wrong_kind, Err(PairRefused));

    let untouched = platform_identity::pairing::find_code(pool, digest(12))
        .await
        .expect("reading back")
        .expect("the pinned code survives a kind refusal");
    assert!(untouched.consumed_at.is_none());

    // An unknown code refuses like every other refusal.
    let mut transaction = pool.begin().await.expect("a transaction");
    let unknown = platform_identity::pairing::redeem(
        &mut transaction,
        &pair_request(digest(99), DeviceKind::Mobile, 102),
    )
    .await
    .expect("redeem runs");
    transaction.commit().await.expect("commit");
    assert_eq!(unknown, Err(PairRefused));

    // And the expired one refuses too.
    let mut transaction = pool.begin().await.expect("a transaction");
    let late = platform_identity::pairing::redeem(
        &mut transaction,
        &pair_request(digest(10), DeviceKind::Mobile, 103),
    )
    .await
    .expect("redeem runs");
    transaction.commit().await.expect("commit");
    assert_eq!(late, Err(PairRefused));

    harness.cleanup().await.expect("cleanup");
}

/// P-3. A leaked but still-secret code has a finite online guessing window. Five mismatched
/// attestations burn it; a sixth (even correct) presentation cannot turn those denials into a
/// device grant.
#[tokio::test]
async fn five_mismatched_attestations_permanently_burn_a_pairing_code() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, creator_session) = a_trusted_context(&harness).await;

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &new_code(
            owner,
            creator_session,
            digest(77),
            Some(DeviceKind::Mobile),
            now(),
        ),
    )
    .await
    .expect("a pairing code");
    transaction.commit().await.expect("commit");

    for attempt in 0..5 {
        let mut transaction = pool.begin().await.expect("a transaction");
        let refused = platform_identity::pairing::redeem(
            &mut transaction,
            &pair_request(digest(77), DeviceKind::ExportAgent, 80 + attempt),
        )
        .await
        .expect("the refusal is not an infrastructure fault");
        transaction.commit().await.expect("commit");
        assert_eq!(refused, Err(PairRefused));
    }

    let mut transaction = pool.begin().await.expect("a transaction");
    let after_budget = platform_identity::pairing::redeem(
        &mut transaction,
        &pair_request(digest(77), DeviceKind::Mobile, 99),
    )
    .await
    .expect("the refusal is not an infrastructure fault");
    transaction.commit().await.expect("commit");
    assert_eq!(
        after_budget,
        Err(PairRefused),
        "the sixth presentation must not create a device after the code's brute-force budget"
    );

    let devices: i64 =
        sqlx::query_scalar("select count(*) from identity.registered_devices where user_id = $1")
            .bind(owner)
            .fetch_one(pool)
            .await
            .expect("counting devices");
    assert_eq!(devices, 0, "a burned code grants no device");

    harness.cleanup().await.expect("cleanup");
}
