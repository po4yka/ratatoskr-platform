//! The `site_deployer` device kind, against a real `PostgreSQL` (workspace change
//! `add-site-gateway-hosting`, spec `site-bundles`, "A deployer device is paired by the owner for
//! one site").
//!
//! A `site_deployer` device deploys one site. The pairing code and the device row carry that one
//! site, and the schema requires it for this kind and refuses it for every other kind.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_identity::pairing::{NewPairingCode, PairRefused, PairRequest};
use platform_identity::refresh::RotationFailure;
use platform_identity::{DeviceKind, NewSession, SecretDigest, SessionKind};
use platform_persistence::PersistenceError;
use platform_persistence::test_support::TestDatabase;

const SITE: &str = "site-a";

/// The instant the whole file runs at, so no assertion depends on the day the suite runs.
fn now() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).expect("a fixed timestamp")
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

fn site_code(
    user_id: uuid::Uuid,
    created_by_session_id: uuid::Uuid,
    code_digest: SecretDigest,
    expected_kind: Option<DeviceKind>,
    site_id: Option<&'static str>,
) -> NewPairingCode<'static> {
    NewPairingCode {
        user_id,
        created_by_session_id,
        expected_kind,
        site_id,
        label: Some("deploy controller"),
        code_digest,
        now: now(),
        expires_at: later(10),
    }
}

fn deployer_request(presented: SecretDigest, seed: u8) -> PairRequest<'static> {
    PairRequest {
        presented,
        declared_kind: DeviceKind::SiteDeployer,
        display_name: Some("deploy controller"),
        device_secret: digest(seed),
        access_token: digest(seed.wrapping_add(1)),
        refresh_token: digest(seed.wrapping_add(2)),
        audience: "edge",
        now: now(),
        access_expires_at: later(60),
        refresh_expires_at: later(60 * 24 * 30),
    }
}

/// The name of the CHECK constraint that refused a statement, or `None` for any other failure.
fn refusing_constraint(error: &PersistenceError) -> Option<String> {
    match error {
        PersistenceError::Query(sqlx::Error::Database(database)) => {
            database.constraint().map(str::to_owned)
        }
        _ => None,
    }
}

async fn pairing_codes_of(pool: &sqlx::PgPool, user_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from identity.pairing_codes where user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("a count")
}

/// Spec `site-bundles`, "A pairing code without a site": a code that pins `site_deployer` and
/// names no site is refused by the rule that binds the site to the kind, and no code is stored. A
/// site on a code of another kind is refused by the same rule.
#[tokio::test]
async fn a_site_deployer_code_requires_a_site() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, creator_session) = a_trusted_context(&harness).await;

    for (label, kind, site) in [
        (
            "site_deployer without a site",
            Some(DeviceKind::SiteDeployer),
            None,
        ),
        (
            "a site on a mobile code",
            Some(DeviceKind::Mobile),
            Some(SITE),
        ),
        ("a site on a code of any kind", None, Some(SITE)),
    ] {
        let mut transaction = pool.begin().await.expect("a transaction");
        let refused = platform_identity::pairing::create_code(
            &mut transaction,
            &site_code(owner, creator_session, digest(1), kind, site),
        )
        .await
        .expect_err(label);
        drop(transaction);
        assert_eq!(
            refusing_constraint(&refused).as_deref(),
            Some("pairing_codes_site_id_matches_kind"),
            "{label} is refused by the site rule: {refused:?}"
        );
    }
    assert_eq!(pairing_codes_of(pool, owner).await, 0, "no code is stored");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-bundles`, "A deployer device is paired by the owner for one site": a code that pins
/// `site_deployer` and one site yields a device of that kind for that site, and the stored device
/// names the site. A code that pins no kind cannot pair a `site_deployer`, because it names no
/// site.
#[tokio::test]
async fn a_site_deployer_pairs_for_one_site() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, creator_session) = a_trusted_context(&harness).await;

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &site_code(owner, creator_session, digest(2), None, None),
    )
    .await
    .expect("a code that pins no kind");
    transaction.commit().await.expect("commit");
    let mut transaction = pool.begin().await.expect("a transaction");
    let unpinned =
        platform_identity::pairing::redeem(&mut transaction, &deployer_request(digest(2), 20))
            .await
            .expect("redeem runs");
    transaction.commit().await.expect("commit");
    assert_eq!(
        unpinned,
        Err(PairRefused),
        "a code without a site cannot pair a site_deployer"
    );

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &site_code(
            owner,
            creator_session,
            digest(3),
            Some(DeviceKind::SiteDeployer),
            Some(SITE),
        ),
    )
    .await
    .expect("a site_deployer code for one site");
    transaction.commit().await.expect("commit");
    let mut transaction = pool.begin().await.expect("a transaction");
    let redeemed =
        platform_identity::pairing::redeem(&mut transaction, &deployer_request(digest(3), 30))
            .await
            .expect("redeem runs")
            .expect("the code pairs the device");
    transaction.commit().await.expect("commit");

    assert_eq!(redeemed.user_id, owner);
    assert_eq!(redeemed.device.kind, DeviceKind::SiteDeployer);
    assert_eq!(redeemed.device.site_id.as_deref(), Some(SITE));
    let stored = platform_identity::device::find_device(pool, redeemed.device.device_id)
        .await
        .expect("reading back")
        .expect("the device exists");
    assert_eq!(stored.kind, DeviceKind::SiteDeployer);
    assert_eq!(
        stored.site_id.as_deref(),
        Some(SITE),
        "the stored device names its site, which Edge mints into its claims"
    );
    assert_eq!(redeemed.session.kind, SessionKind::Device);
    assert_eq!(redeemed.session.device_id, Some(redeemed.device.device_id));

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-bundles`, "A revoked deployer device": after the owner revokes a `site_deployer`
/// device, its refresh token rotates nothing and its device secret opens no session.
#[tokio::test]
async fn a_revoked_site_deployer_cannot_refresh() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, creator_session) = a_trusted_context(&harness).await;

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::pairing::create_code(
        &mut transaction,
        &site_code(
            owner,
            creator_session,
            digest(4),
            Some(DeviceKind::SiteDeployer),
            Some(SITE),
        ),
    )
    .await
    .expect("a site_deployer code for one site");
    transaction.commit().await.expect("commit");
    let mut transaction = pool.begin().await.expect("a transaction");
    let redeemed =
        platform_identity::pairing::redeem(&mut transaction, &deployer_request(digest(4), 40))
            .await
            .expect("redeem runs")
            .expect("the code pairs the device");
    transaction.commit().await.expect("commit");
    let device_id = redeemed.device.device_id;

    let mut transaction = pool.begin().await.expect("a transaction");
    platform_identity::device::revoke_device(&mut transaction, device_id, later(1))
        .await
        .expect("the device is revoked");
    transaction.commit().await.expect("commit");

    let mut transaction = pool.begin().await.expect("a transaction");
    let rotated = platform_identity::refresh::rotate_session(
        &mut transaction,
        digest(42),
        digest(50),
        digest(51),
        later(2),
        later(62),
        later(60 * 24 * 30),
    )
    .await
    .expect("rotation runs");
    transaction.commit().await.expect("commit");
    assert_eq!(
        rotated,
        Err(RotationFailure::SessionNotLive),
        "the refresh token of a revoked device rotates nothing"
    );

    let opened = platform_identity::device::authenticate_device(pool, device_id, digest(40))
        .await
        .expect("the device check runs");
    assert_eq!(opened, None, "the device secret opens no session");

    harness.cleanup().await.expect("cleanup");
}
