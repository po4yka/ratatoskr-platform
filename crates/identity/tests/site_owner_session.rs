//! The `site_owner` session, against a real `PostgreSQL` (workspace change
//! `add-site-owner-editing-and-live`, spec `site-owner-session`): it authenticates only at the
//! audience of its own site, ends 12 hours after issue or 1 hour after its last use, and a new
//! sign-in does not extend an old session.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_identity::session::{self, site_audience};
use platform_identity::{NewSession, SecretDigest};
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

const SITE: &str = "site-a";
const OTHER_SITE: &str = "site-b";

/// The instant the whole file runs at; the clock is a parameter of the library.
fn now() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).expect("a fixed timestamp")
}

fn minutes(count: i64) -> jiff::Timestamp {
    now() + jiff::SignedDuration::from_mins(count)
}

/// A `site_owner` session of a new user for [`SITE`], issued at `at`, whose credential digest is
/// `token`.
async fn site_session(pool: &sqlx::PgPool, token: SecretDigest, at: jiff::Timestamp) -> Uuid {
    let user = platform_identity::user::create_user(pool, at)
        .await
        .expect("a user");
    let audience = site_audience(SITE);
    session::create_session(
        pool,
        &NewSession::site_owner(user.user_id, &audience, token, at),
    )
    .await
    .expect("a site_owner session")
    .session_id
}

/// Whether `token` authenticates at `audience` at `at`, and if it does, record the use.
async fn present(
    pool: &sqlx::PgPool,
    token: SecretDigest,
    audience: &str,
    at: jiff::Timestamp,
) -> bool {
    let found = session::authenticate(pool, token, audience, at)
        .await
        .expect("authentication runs");
    if let Some(found) = &found {
        session::touch_last_seen(
            pool,
            found.session_id,
            None,
            at,
            jiff::SignedDuration::from_secs(60),
        )
        .await
        .expect("the use is recorded");
    }
    found.is_some()
}

/// Spec `site-owner-session`, "The owner session calls another site": at another site's audience
/// the session is no credential at all.
#[tokio::test]
async fn other_site_is_anonymous() {
    let harness = TestDatabase::create().await.expect("a test database");
    let token = SecretDigest::new([1; 32]);
    site_session(harness.pool(), token, now()).await;

    assert!(present(harness.pool(), token, &site_audience(SITE), minutes(1)).await);
    assert!(
        !present(
            harness.pool(),
            token,
            &site_audience(OTHER_SITE),
            minutes(2)
        )
        .await,
        "the session of site-a authenticates at site-b"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "The owner session calls a general route": the general routes
/// authenticate at the audience of Edge, where a site session is no credential.
#[tokio::test]
async fn general_route_is_unauthenticated() {
    let harness = TestDatabase::create().await.expect("a test database");
    let token = SecretDigest::new([2; 32]);
    site_session(harness.pool(), token, now()).await;

    assert!(
        !present(harness.pool(), token, "edge", minutes(1)).await,
        "a site session authenticates on a general route"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "The owner session calls a management route of its own site": a
/// management route authenticates at the audience of Edge too.
#[tokio::test]
async fn management_route_is_unauthenticated() {
    let harness = TestDatabase::create().await.expect("a test database");
    let token = SecretDigest::new([3; 32]);
    site_session(harness.pool(), token, now()).await;

    assert!(
        !present(harness.pool(), token, "edge", minutes(1)).await,
        "a site session authenticates on a management route"
    );
    assert!(present(harness.pool(), token, &site_audience(SITE), minutes(2)).await);

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "An idle session ends": 61 minutes without use end the session.
#[tokio::test]
async fn idle_after_sixty_one_minutes_fails() {
    let harness = TestDatabase::create().await.expect("a test database");
    let token = SecretDigest::new([4; 32]);
    site_session(harness.pool(), token, now()).await;

    assert!(
        !present(harness.pool(), token, &site_audience(SITE), minutes(61)).await,
        "a session unused for 61 minutes authenticates"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A session reaches its absolute lifetime": a session used every 10
/// minutes ends 12 hours after issue.
#[tokio::test]
async fn absolute_lifetime_twelve_hours_fails() {
    let harness = TestDatabase::create().await.expect("a test database");
    let token = SecretDigest::new([5; 32]);
    site_session(harness.pool(), token, now()).await;
    let audience = site_audience(SITE);

    for step in 1..72 {
        assert!(
            present(harness.pool(), token, &audience, minutes(step * 10)).await,
            "the session is live after {} minutes of use",
            step * 10
        );
    }
    assert!(
        !present(harness.pool(), token, &audience, minutes(12 * 60 + 1)).await,
        "the session authenticates 12 hours and 1 minute after issue"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A second sign-in does not extend the first".
#[tokio::test]
async fn new_sign_in_does_not_extend_old_session() {
    let harness = TestDatabase::create().await.expect("a test database");
    let first = SecretDigest::new([6; 32]);
    let second = SecretDigest::new([7; 32]);
    site_session(harness.pool(), first, now()).await;
    let audience = site_audience(SITE);
    for step in 1..72 {
        assert!(present(harness.pool(), first, &audience, minutes(step * 10)).await);
    }
    site_session(harness.pool(), second, minutes(11 * 60)).await;
    // The second session is used too, so that its idle limit plays no part.
    assert!(present(harness.pool(), second, &audience, minutes(11 * 60 + 30)).await);

    assert!(
        !present(harness.pool(), first, &audience, minutes(12 * 60 + 1)).await,
        "the second sign-in extended the first session"
    );
    assert!(
        present(harness.pool(), second, &audience, minutes(12 * 60 + 1)).await,
        "the second session authenticates"
    );

    harness.cleanup().await.expect("cleanup");
}
