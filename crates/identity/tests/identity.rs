//! The identity schema, against a real `PostgreSQL`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_identity::NewSession;
use platform_identity::session::RefreshFailure;
use platform_identity::{
    AuditEvent, AuditOutcome, DeviceKind, IdentityProvider, RevocationReason, RevocationSubject,
    SecretDigest, SessionKind, UserStatus,
};
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

/// I-1. A provider identity always resolves to the same internal user, however often it is
/// presented. This is what stops a retried sign-in creating a second account.
#[tokio::test]
async fn linking_a_provider_identity_is_idempotent() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let first = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let second = platform_identity::user::create_user(pool, now())
        .await
        .expect("another user");

    let link = platform_identity::user::link_external_identity(
        pool,
        first.user_id,
        IdentityProvider::Telegram,
        "1234567",
        now(),
    )
    .await
    .expect("the first link");
    assert_eq!(link.user_id, first.user_id);

    // A second attempt naming a DIFFERENT internal user must not move the mapping.
    let again = platform_identity::user::link_external_identity(
        pool,
        second.user_id,
        IdentityProvider::Telegram,
        "1234567",
        now(),
    )
    .await
    .expect("the second link");
    assert_eq!(
        again.user_id, first.user_id,
        "a provider identity must not silently move between internal users"
    );

    let resolved = platform_identity::user::find_user_by_external_identity(
        pool,
        IdentityProvider::Telegram,
        "1234567",
    )
    .await
    .expect("resolving");
    assert_eq!(resolved, Some(first.user_id));

    harness.cleanup().await.expect("cleanup");
}

/// I-2. Liveness is computed from revocation and expiry, not stored.
#[tokio::test]
async fn a_session_is_live_until_it_is_revoked_or_expires() {
    let harness = TestDatabase::create().await.expect("a test database");
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
            // These tests exercise the session lifecycle, not authentication; a session with no
            // credential simply never authenticates, which is the safe default.
            token: None,
            issued_at: now(),
            expires_at: later(60),
        },
    )
    .await
    .expect("a session");

    assert!(session.is_live_at(now()));
    assert!(
        !session.is_live_at(later(61)),
        "a session must not outlive its expiry"
    );

    assert!(
        platform_identity::session::revoke_session(pool, session.session_id, now())
            .await
            .expect("revoking")
    );
    let revoked = platform_identity::session::find_session(pool, session.session_id)
        .await
        .expect("reading")
        .expect("the session");
    assert!(!revoked.is_live_at(now()));

    // Revoking twice is a no-op, and must not move the recorded instant.
    assert!(
        !platform_identity::session::revoke_session(pool, session.session_id, later(1))
            .await
            .expect("revoking again"),
        "a second revocation must not claim to have done anything"
    );
    let again = platform_identity::session::find_session(pool, session.session_id)
        .await
        .expect("reading")
        .expect("the session");
    assert_eq!(again.revoked_at, revoked.revoked_at);

    harness.cleanup().await.expect("cleanup");
}

/// I-3. Revoking a device revokes every session bound to it, in one transaction. Leaving those
/// sessions live is the privilege escalation the threat model names.
#[tokio::test]
async fn revoking_a_device_revokes_its_sessions() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let device = platform_identity::device::register_device(
        pool,
        user.user_id,
        DeviceKind::Mobile,
        Some("a phone"),
        digest(7),
        now(),
    )
    .await
    .expect("a device");

    for _ in 0..3 {
        platform_identity::session::create_session(
            pool,
            &NewSession {
                user_id: user.user_id,
                kind: SessionKind::Device,
                device_id: Some(device.device_id),
                audience: "edge",
                // These tests exercise the session lifecycle, not authentication; a session with no
                // credential simply never authenticates, which is the safe default.
                token: None,
                issued_at: now(),
                expires_at: later(60),
            },
        )
        .await
        .expect("a device session");
    }

    let mut transaction = pool.begin().await.expect("a transaction");
    let revoked =
        platform_identity::device::revoke_device(&mut transaction, device.device_id, now())
            .await
            .expect("revoking the device");
    transaction.commit().await.expect("commit");
    assert_eq!(revoked, 3, "every session of the device must be revoked");

    let live: i64 = sqlx::query_scalar(
        "select count(*) from identity.sessions where device_id = $1 and revoked_at is null",
    )
    .bind(device.device_id)
    .fetch_one(pool)
    .await
    .expect("counting");
    assert_eq!(live, 0);

    // A revoked device no longer verifies, and an unknown one is indistinguishable from it.
    assert!(
        !platform_identity::device::verify_device_secret(pool, device.device_id, digest(7))
            .await
            .expect("verifying")
    );

    harness.cleanup().await.expect("cleanup");
}

/// I-4. A device secret verifies only against its own digest.
#[tokio::test]
async fn a_device_secret_verifies_only_when_it_matches() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let device = platform_identity::device::register_device(
        pool,
        user.user_id,
        DeviceKind::ExportAgent,
        None,
        digest(1),
        now(),
    )
    .await
    .expect("a device");

    assert!(
        platform_identity::device::verify_device_secret(pool, device.device_id, digest(1))
            .await
            .expect("verifying")
    );
    assert!(
        !platform_identity::device::verify_device_secret(pool, device.device_id, digest(2))
            .await
            .expect("verifying")
    );

    harness.cleanup().await.expect("cleanup");
}

/// I-5. Rotation spends the presented token and issues its successor; replaying a spent token is
/// reported as a replay, not as an unknown token, because the two mean different things.
#[tokio::test]
async fn refresh_rotation_detects_a_replayed_token() {
    let harness = TestDatabase::create().await.expect("a test database");
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
            // These tests exercise the session lifecycle, not authentication; a session with no
            // credential simply never authenticates, which is the safe default.
            token: None,
            issued_at: now(),
            expires_at: later(60),
        },
    )
    .await
    .expect("a session");
    platform_identity::session::issue_refresh_token(
        pool,
        session.session_id,
        digest(10),
        now(),
        later(30),
    )
    .await
    .expect("the first token");

    let mut transaction = pool.begin().await.expect("a transaction");
    let rotated = platform_identity::session::rotate_refresh_token(
        &mut transaction,
        digest(10),
        digest(11),
        now(),
        later(30),
    )
    .await
    .expect("rotating")
    .expect("the rotation succeeds");
    transaction.commit().await.expect("commit");
    assert_eq!(rotated.session_id, session.session_id);

    // Presenting the spent token again is a replay.
    let mut transaction = pool.begin().await.expect("a transaction");
    let replay = platform_identity::session::rotate_refresh_token(
        &mut transaction,
        digest(10),
        digest(12),
        now(),
        later(30),
    )
    .await
    .expect("rotating");
    transaction.rollback().await.expect("rollback");
    assert_eq!(replay.unwrap_err(), RefreshFailure::Replayed);

    // An unknown digest is not a replay.
    let mut transaction = pool.begin().await.expect("a transaction");
    let unknown = platform_identity::session::rotate_refresh_token(
        &mut transaction,
        digest(99),
        digest(13),
        now(),
        later(30),
    )
    .await
    .expect("rotating");
    transaction.rollback().await.expect("rollback");
    assert_eq!(unknown.unwrap_err(), RefreshFailure::Unknown);

    harness.cleanup().await.expect("cleanup");
}

/// I-6. A rotation against a revoked session is refused even though the token itself is fine.
#[tokio::test]
async fn rotation_is_refused_when_the_session_is_not_live() {
    let harness = TestDatabase::create().await.expect("a test database");
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
            // These tests exercise the session lifecycle, not authentication; a session with no
            // credential simply never authenticates, which is the safe default.
            token: None,
            issued_at: now(),
            expires_at: later(60),
        },
    )
    .await
    .expect("a session");
    platform_identity::session::issue_refresh_token(
        pool,
        session.session_id,
        digest(20),
        now(),
        later(30),
    )
    .await
    .expect("a token");
    platform_identity::session::revoke_session(pool, session.session_id, now())
        .await
        .expect("revoking");

    let mut transaction = pool.begin().await.expect("a transaction");
    let refused = platform_identity::session::rotate_refresh_token(
        &mut transaction,
        digest(20),
        digest(21),
        now(),
        later(30),
    )
    .await
    .expect("rotating");
    transaction.rollback().await.expect("rollback");
    assert_eq!(refused.unwrap_err(), RefreshFailure::SessionNotLive);

    harness.cleanup().await.expect("cleanup");
}

/// I-7. Revoking every session of a user is one statement, and a revocation record explains why.
#[tokio::test]
async fn a_user_wide_revocation_ends_every_session_and_is_recorded() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    for _ in 0..4 {
        platform_identity::session::create_session(
            pool,
            &NewSession {
                user_id: user.user_id,
                kind: SessionKind::Browser,
                device_id: None,
                audience: "edge",
                // These tests exercise the session lifecycle, not authentication; a session with no
                // credential simply never authenticates, which is the safe default.
                token: None,
                issued_at: now(),
                expires_at: later(60),
            },
        )
        .await
        .expect("a session");
    }

    let ended = platform_identity::session::revoke_all_sessions_of_user(pool, user.user_id, now())
        .await
        .expect("revoking");
    assert_eq!(ended, 4);

    platform_identity::record_revocation(
        pool,
        RevocationSubject::User,
        user.user_id,
        RevocationReason::SuspectedCompromise,
        None,
        now(),
    )
    .await
    .expect("recording");

    let recorded =
        platform_identity::count_revocations(pool, RevocationSubject::User, user.user_id)
            .await
            .expect("counting");
    assert_eq!(recorded, 1);

    // A suspended user may not authenticate, which the status alone answers.
    platform_identity::user::set_user_status(pool, user.user_id, UserStatus::Suspended, now())
        .await
        .expect("suspending");
    let suspended = platform_identity::user::find_user(pool, user.user_id)
        .await
        .expect("reading")
        .expect("the user");
    assert!(!suspended.status.may_authenticate());

    harness.cleanup().await.expect("cleanup");
}

/// I-8. The audit trail joins to a support conversation on the correlation identifier, and refuses
/// a value that is not in the namespaced wire form.
#[tokio::test]
async fn an_audit_record_requires_a_namespaced_correlation_id() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let correlation = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";

    platform_identity::audit::record(
        pool,
        &AuditEvent {
            audit_event_id: uuid::Uuid::now_v7(),
            actor_user_id: Some(user.user_id),
            actor_session_id: None,
            action: "session.create".to_owned(),
            target_kind: "session".to_owned(),
            target_id: None,
            outcome: AuditOutcome::Allowed,
            correlation_id: correlation.to_owned(),
        },
        now(),
    )
    .await
    .expect("recording");

    assert_eq!(
        platform_identity::audit::count_by_correlation(pool, correlation)
            .await
            .expect("counting"),
        1
    );

    let malformed = platform_identity::audit::record(
        pool,
        &AuditEvent {
            audit_event_id: uuid::Uuid::now_v7(),
            actor_user_id: None,
            actor_session_id: None,
            action: "session.create".to_owned(),
            target_kind: "session".to_owned(),
            target_id: None,
            outcome: AuditOutcome::Denied,
            correlation_id: "not-namespaced".to_owned(),
        },
        now(),
    )
    .await;
    assert!(
        malformed.is_err(),
        "an audit record must carry the same correlation form the client saw"
    );

    harness.cleanup().await.expect("cleanup");
}
