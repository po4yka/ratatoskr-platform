//! Session and device lifecycle queries: listing, liveness touches, rotation, cascading deletion.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_identity::device::ListedDevice;
use platform_identity::session::RotationFailure;
use platform_identity::{DeviceKind, NewSession, SecretDigest, SessionKind};
use platform_persistence::test_support::TestDatabase;

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn ago(minutes: i64) -> jiff::Timestamp {
    now() - jiff::SignedDuration::from_mins(minutes)
}

fn later(minutes: i64) -> jiff::Timestamp {
    now() + jiff::SignedDuration::from_mins(minutes)
}

fn digest(seed: u8) -> SecretDigest {
    SecretDigest::new([seed; 32])
}

async fn a_user(harness: &TestDatabase) -> uuid::Uuid {
    platform_identity::user::create_user(harness.pool(), now())
        .await
        .expect("a user")
        .user_id
}

#[allow(clippy::too_many_arguments)]
async fn a_session(
    harness: &TestDatabase,
    user_id: uuid::Uuid,
    kind: SessionKind,
    device_id: Option<uuid::Uuid>,
    token: Option<SecretDigest>,
    issued_at: jiff::Timestamp,
    expires_at: jiff::Timestamp,
) -> uuid::Uuid {
    platform_identity::session::create_session(
        harness.pool(),
        &NewSession {
            user_id,
            kind,
            device_id,
            audience: "edge",
            token,
            issued_at,
            expires_at,
        },
    )
    .await
    .expect("a session")
    .session_id
}

/// The listing fixture: two owners, three live sessions for alice, one dead of each cause.
///
/// Answers (alice, bob, oldest, middle, newest).
#[allow(clippy::too_many_arguments)]
async fn seeded_sessions(
    harness: &TestDatabase,
) -> (uuid::Uuid, uuid::Uuid, uuid::Uuid, uuid::Uuid, uuid::Uuid) {
    let pool = harness.pool();
    let alice = a_user(harness).await;
    let bob = a_user(harness).await;
    let device = platform_identity::device::register_device(
        pool,
        alice,
        DeviceKind::BrowserExtension,
        Some("the extension"),
        digest(1),
        now(),
    )
    .await
    .expect("a device");

    let oldest = a_session(
        harness,
        alice,
        SessionKind::Device,
        Some(device.device_id),
        None,
        ago(180),
        later(60),
    )
    .await;
    let middle = a_session(
        harness,
        alice,
        SessionKind::TelegramMiniApp,
        None,
        None,
        ago(120),
        later(60),
    )
    .await;
    let newest = a_session(
        harness,
        alice,
        SessionKind::Browser,
        None,
        None,
        ago(60),
        later(60),
    )
    .await;
    let revoked = a_session(
        harness,
        alice,
        SessionKind::Browser,
        None,
        None,
        ago(50),
        later(60),
    )
    .await;
    platform_identity::session::revoke_session(pool, revoked, ago(10))
        .await
        .expect("revoking");
    let _expired = a_session(
        harness,
        alice,
        SessionKind::Browser,
        None,
        None,
        ago(200),
        ago(100),
    )
    .await;
    let _bobs = a_session(
        harness,
        bob,
        SessionKind::Browser,
        None,
        None,
        ago(30),
        later(60),
    )
    .await;

    let _ = device.device_id;
    (alice, bob, oldest, middle, newest)
}

#[tokio::test]
async fn session_listing_orders_pages_and_touches_liveness() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (alice, bob, oldest, middle, newest) = seeded_sessions(&harness).await;
    let app_free_pool = pool;

    let page_one =
        platform_identity::session::list_live_sessions(app_free_pool, alice, now(), None, 2)
            .await
            .expect("the first page");
    assert_eq!(page_one.len(), 2, "the limit bounds the page");
    assert_eq!(page_one[0].session_id, newest);
    assert_eq!(page_one[1].session_id, middle);
    assert!(
        page_one.iter().all(|s| s.session_id != bob),
        "another user's session never appears"
    );
    assert_eq!(page_one[0].kind, SessionKind::Browser);

    let last = page_one.last().expect("non-empty");
    let page_two = platform_identity::session::list_live_sessions(
        pool,
        alice,
        now(),
        Some((last.issued_at, last.session_id)),
        2,
    )
    .await
    .expect("the second page");
    let ids: Vec<_> = page_two.iter().map(|s| s.session_id).collect();
    assert_eq!(
        ids,
        vec![oldest],
        "the walk continues exactly where the cursor stopped"
    );

    // The bound device's identity rides along.
    let with_device = page_two
        .iter()
        .find(|s| s.session_id == oldest)
        .expect("the device session");
    let device_ref = with_device.device.as_ref().expect("it names its device");
    assert_eq!(device_ref.display_name.as_deref(), Some("the extension"));

    harness.cleanup().await.expect("cleanup");
}

/// A device session with its first refresh link: the fixture every rotation test starts from.
async fn a_device_session_with_link(
    harness: &TestDatabase,
) -> (uuid::Uuid, uuid::Uuid, uuid::Uuid) {
    let pool = harness.pool();
    let owner = a_user(harness).await;
    let device = platform_identity::device::register_device(
        pool,
        owner,
        DeviceKind::Mobile,
        None,
        digest(1),
        now(),
    )
    .await
    .expect("a device");
    let issued = ago(30);
    let session = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: owner,
            kind: SessionKind::Device,
            device_id: Some(device.device_id),
            audience: "edge",
            token: Some(digest(10)),
            issued_at: issued,
            expires_at: issued + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    platform_identity::session::issue_refresh_token(
        pool,
        session.session_id,
        digest(11),
        issued,
        later(60 * 24 * 30),
    )
    .await
    .expect("a refresh link");
    (owner, session.session_id, device.device_id)
}

/// L-1b. The liveness touch is throttled to one write per interval.
#[tokio::test]
async fn liveness_touches_are_throttled() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let alice = a_user(&harness).await;
    let device = platform_identity::device::register_device(
        pool,
        alice,
        DeviceKind::Mobile,
        None,
        digest(2),
        now(),
    )
    .await
    .expect("a device");
    let session_id = a_session(
        &harness,
        alice,
        SessionKind::Device,
        Some(device.device_id),
        None,
        ago(5),
        later(60),
    )
    .await;

    // Liveness touches are throttled to one write per interval.
    platform_identity::session::touch_last_seen(
        pool,
        session_id,
        Some(device.device_id),
        now(),
        jiff::SignedDuration::from_secs(60),
    )
    .await
    .expect("the first touch lands");
    let after_first = platform_identity::session::find_session(pool, session_id)
        .await
        .expect("reading back")
        .expect("it exists")
        .last_seen_at;
    assert!(after_first.is_some(), "the touch wrote an instant");

    platform_identity::session::touch_last_seen(
        pool,
        session_id,
        Some(device.device_id),
        now() + jiff::SignedDuration::from_secs(30),
        jiff::SignedDuration::from_secs(60),
    )
    .await
    .expect("an early second touch runs");
    let after_second = platform_identity::session::find_session(pool, session_id)
        .await
        .expect("reading back")
        .expect("it exists")
        .last_seen_at;
    assert_eq!(
        after_first, after_second,
        "within the interval the touch must not move last-seen"
    );

    platform_identity::session::touch_last_seen(
        pool,
        session_id,
        Some(device.device_id),
        later(2),
        jiff::SignedDuration::from_secs(60),
    )
    .await
    .expect("a late third touch runs");
    let after_third = platform_identity::session::find_session(pool, session_id)
        .await
        .expect("reading back")
        .expect("it exists")
        .last_seen_at;
    assert_ne!(
        after_first, after_third,
        "past the interval the touch advances last-seen"
    );

    harness.cleanup().await.expect("cleanup");
}

/// L-2. Rotation swaps both credentials atomically; replaying a spent link burns the family.
#[tokio::test]
async fn credential_rotation_swaps_access_and_replay_burns_the_family() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, session_id, _device_id) = a_device_session_with_link(&harness).await;
    let session = platform_identity::session::find_session(pool, session_id)
        .await
        .expect("reading back")
        .expect("it exists");

    let mut transaction = pool.begin().await.expect("a transaction");
    let rotated = platform_identity::session::rotate_session(
        &mut transaction,
        digest(11),
        digest(12),
        digest(13),
        now(),
        later(60),
        later(60 * 24 * 30),
    )
    .await
    .expect("rotation runs")
    .expect("a live link rotates");
    transaction.commit().await.expect("commit");
    assert_eq!(rotated.refresh.session_id, session.session_id);

    // The old access credential is dead, the new one authenticates, and the window moved out.
    let old_access = platform_identity::session::authenticate(pool, digest(10), "edge", now())
        .await
        .expect("authenticating");
    assert!(old_access.is_none(), "the replaced credential must be dead");
    let new_access = platform_identity::session::authenticate(pool, digest(13), "edge", now())
        .await
        .expect("authenticating")
        .expect("the swapped-in credential works");
    assert!(
        new_access.expires_at > session.expires_at,
        "rotation extends the window"
    );

    // The chain advanced: the presented link is consumed and names its successor.
    let mut transaction = pool.begin().await.expect("a transaction");
    let outcome = platform_identity::session::rotate_session(
        &mut transaction,
        digest(11),
        digest(14),
        digest(15),
        now(),
        later(60),
        later(60 * 24 * 30),
    )
    .await
    .expect("replay runs");
    match outcome {
        Err(RotationFailure::Replayed {
            session_id,
            user_id,
        }) => {
            assert_eq!(session_id, session.session_id);
            assert_eq!(user_id, owner);
        }
        other => panic!("expected a replay verdict, got {other:?}"),
    }
    transaction.commit().await.expect("commit");

    // Replay burned the family: nothing authenticates any more.
    let after_burn = platform_identity::session::authenticate(pool, digest(13), "edge", now())
        .await
        .expect("authenticating");
    assert!(
        after_burn.is_none(),
        "a replayed family revokes its session"
    );
    let recorded = platform_identity::count_revocations(
        pool,
        platform_identity::RevocationSubject::Session,
        session.session_id,
    )
    .await
    .expect("counting revocations");
    assert_eq!(recorded, 1, "the burn records why");

    // An unknown link refuses without touching anything.
    let mut transaction = pool.begin().await.expect("a transaction");
    let unknown = platform_identity::session::rotate_session(
        &mut transaction,
        digest(99),
        digest(16),
        digest(17),
        now(),
        later(60),
        later(60 * 24 * 30),
    )
    .await
    .expect("rotation runs");
    transaction.commit().await.expect("commit");
    assert!(matches!(unknown, Err(RotationFailure::Unknown)));

    harness.cleanup().await.expect("cleanup");
}

/// A fixture for the device-cascade tests: alice holds a device with two sessions, bob one.
async fn seeded_devices(
    harness: &TestDatabase,
) -> (uuid::Uuid, uuid::Uuid, Vec<uuid::Uuid>, uuid::Uuid) {
    let pool = harness.pool();
    let alice = a_user(harness).await;
    let bob = a_user(harness).await;

    let hers = platform_identity::device::register_device(
        pool,
        alice,
        DeviceKind::ExportAgent,
        Some("agent"),
        digest(20),
        now(),
    )
    .await
    .expect("her device")
    .device_id;
    let his = platform_identity::device::register_device(
        pool,
        bob,
        DeviceKind::ExportAgent,
        None,
        digest(21),
        now(),
    )
    .await
    .expect("his device")
    .device_id;

    let s1 = a_session(
        harness,
        alice,
        SessionKind::Device,
        Some(hers),
        Some(digest(30)),
        ago(5),
        later(60),
    )
    .await;
    let s2 = a_session(
        harness,
        alice,
        SessionKind::Device,
        Some(hers),
        Some(digest(31)),
        ago(3),
        later(60),
    )
    .await;
    let _foreign = a_session(
        harness,
        bob,
        SessionKind::Device,
        Some(his),
        Some(digest(32)),
        ago(4),
        later(60),
    )
    .await;

    (alice, hers, vec![s1, s2], bob)
}

/// L-3a. The active-device listing is the owner's live devices only.
#[tokio::test]
async fn device_listing_stays_scoped_and_excludes_revoked() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (alice, hers, _sessions, bob) = seeded_devices(&harness).await;

    let before = platform_identity::device::list_active_devices(pool, alice, None, 10)
        .await
        .expect("listing");
    assert_eq!(before.len(), 1);
    assert_eq!(
        before[0],
        ListedDevice {
            device_id: hers,
            kind: DeviceKind::ExportAgent,
            display_name: Some("agent".into()),
            created_at: before[0].created_at,
            last_seen_at: before[0].last_seen_at,
        }
    );
    let bobs_view = platform_identity::device::list_active_devices(pool, bob, None, 10)
        .await
        .expect("listing");
    assert_eq!(bobs_view.len(), 1);
    assert_ne!(bobs_view[0].device_id, hers);

    // After revocation the device leaves the active listing entirely.
    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::device::revoke_device(&mut transaction, hers, now())
        .await
        .expect("revoking");
    transaction.commit().await.expect("commit");
    let after = platform_identity::device::list_active_devices(pool, alice, None, 10)
        .await
        .expect("listing");
    assert!(
        after.is_empty(),
        "a revoked device leaves the active listing"
    );

    harness.cleanup().await.expect("cleanup");
}

/// L-3b. Revoking a device answers with every session it killed.
#[tokio::test]
async fn device_revocation_answers_with_the_sessions_it_kills() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (_alice, hers, sessions, _bob) = seeded_devices(&harness).await;

    let mut expected = sessions;
    expected.sort();

    let mut transaction = pool.begin().await.expect("a transaction");
    let killed = platform_identity::device::revoke_device(&mut transaction, hers, now())
        .await
        .expect("revoking");
    transaction.commit().await.expect("commit");

    let mut killed_ids = killed;
    killed_ids.sort();
    assert_eq!(
        killed_ids, expected,
        "the answer names every session it revoked"
    );

    for credential in [digest(30), digest(31)] {
        let live = platform_identity::session::authenticate(pool, credential, "edge", now())
            .await
            .expect("authenticating");
        assert!(
            live.is_none(),
            "a cascaded session must stop authenticating"
        );
    }

    harness.cleanup().await.expect("cleanup");
}
