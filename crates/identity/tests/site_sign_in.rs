//! Site sign-in codes, against a real `PostgreSQL` (workspace change
//! `add-site-owner-editing-and-live`, spec `site-owner-session`): single use, a PKCE verifier, one
//! site, sixty seconds, a primary session to approve, and only a digest stored.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::fmt::Write as _;

use platform_identity::site_sign_in::{
    self, NewSignInCode, SignInRedeem, Unauthenticated, challenge_of,
};
use platform_identity::{DeviceKind, NewSession, SecretDigest, SessionKind};
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

const SITE: &str = "site-a";
const OTHER_SITE: &str = "site-b";
const VERIFIER: &str = "a-pkce-verifier-of-the-site-0000000000000000000";
const OWNER_GRANT: &str = "platform.owner";

/// The instant the whole file runs at; the clock is a parameter of the library, so a later instant
/// is the paused clock of the task.
fn now() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).expect("a fixed timestamp")
}

fn later(seconds: i64) -> jiff::Timestamp {
    now() + jiff::SignedDuration::from_secs(seconds)
}

/// A user with a live session of `kind`. A device session gets a registered device.
async fn context(harness: &TestDatabase, kind: SessionKind) -> (Uuid, Uuid) {
    let pool = harness.pool();
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let device_id = if kind == SessionKind::Device {
        Some(
            platform_identity::device::register_device(
                pool,
                user.user_id,
                DeviceKind::Mobile,
                None,
                SecretDigest::new([7; 32]),
                now(),
            )
            .await
            .expect("a device")
            .device_id,
        )
    } else {
        None
    };
    let session = platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind,
            device_id,
            audience: "edge",
            token: None,
            issued_at: now(),
            expires_at: later(3600),
        },
    )
    .await
    .expect("a session");
    platform_identity::grant::grant(pool, user.user_id, OWNER_GRANT, now(), None)
        .await
        .expect("the owner grant");
    (user.user_id, session.session_id)
}

/// Issue a code for [`SITE`] from the session `session` of `user`.
async fn issue(
    harness: &TestDatabase,
    user: Uuid,
    session: Uuid,
) -> Result<String, site_sign_in::IssueRefused> {
    let challenge = challenge_of(VERIFIER);
    let mut transaction = harness.pool().begin().await.expect("a transaction");
    let issued = site_sign_in::issue(
        &mut transaction,
        &NewSignInCode {
            user_id: user,
            created_by_session_id: session,
            site_id: SITE,
            challenge: &challenge,
            return_path: "/notes/start",
            hostname: "garden.example.org",
            now: now(),
        },
    )
    .await
    .expect("issue runs");
    transaction.commit().await.expect("commit");
    issued
}

async fn redeem(
    harness: &TestDatabase,
    site: &str,
    code: &str,
    verifier: &str,
    at: jiff::Timestamp,
) -> Result<site_sign_in::Redeemed, Unauthenticated> {
    let mut transaction = harness.pool().begin().await.expect("a transaction");
    let redeemed = site_sign_in::redeem(
        &mut transaction,
        &SignInRedeem {
            site_id: site,
            code,
            verifier,
            now: at,
        },
    )
    .await
    .expect("redeem runs");
    transaction.commit().await.expect("commit");
    redeemed
}

/// Spec `site-owner-session`, "A redeemed code is presented again".
#[tokio::test]
async fn code_is_single_use() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Browser).await;
    let code = issue(&harness, owner, session).await.expect("a code");

    let first = redeem(&harness, SITE, &code, VERIFIER, later(5))
        .await
        .expect("the first presentation redeems");
    assert_eq!(first.user_id, owner);
    assert_eq!(first.site_id, SITE);
    assert_eq!(first.return_path, "/notes/start");
    assert_eq!(first.hostname, "garden.example.org");
    assert_eq!(
        redeem(&harness, SITE, &code, VERIFIER, later(6)).await,
        Err(Unauthenticated),
        "a second presentation is refused"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A code is presented without the matching verifier".
#[tokio::test]
async fn verifier_mismatch_is_indistinguishable_from_unknown() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Browser).await;
    let code = issue(&harness, owner, session).await.expect("a code");

    let mismatch = redeem(&harness, SITE, &code, "another-verifier", later(5)).await;
    let unknown = redeem(&harness, SITE, "an-unknown-code", VERIFIER, later(5)).await;
    assert_eq!(
        mismatch,
        Err(Unauthenticated),
        "a wrong verifier is refused"
    );
    assert_eq!(
        mismatch, unknown,
        "and the refusal equals that of an unknown code"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A code is presented at another site".
#[tokio::test]
async fn code_for_another_site_is_indistinguishable_from_unknown() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Browser).await;
    let code = issue(&harness, owner, session).await.expect("a code");

    let elsewhere = redeem(&harness, OTHER_SITE, &code, VERIFIER, later(5)).await;
    let unknown = redeem(&harness, OTHER_SITE, "an-unknown-code", VERIFIER, later(5)).await;
    assert_eq!(elsewhere, Err(Unauthenticated), "another site is refused");
    assert_eq!(
        elsewhere, unknown,
        "and the refusal equals that of an unknown code"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A code expires": 61 seconds after issue the code is refused.
#[tokio::test]
async fn code_expires_after_sixty_seconds() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Browser).await;
    let code = issue(&harness, owner, session).await.expect("a code");

    assert_eq!(
        redeem(&harness, SITE, &code, VERIFIER, later(61)).await,
        Err(Unauthenticated),
        "a code 61 seconds old is refused"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-session`, "A device session asks for a code": only a primary session approves
/// a sign-in, and no code is stored.
#[tokio::test]
async fn device_session_cannot_issue_code() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Device).await;

    assert_eq!(
        issue(&harness, owner, session).await,
        Err(site_sign_in::IssueRefused),
        "a device session cannot approve a sign-in"
    );
    let stored: i64 = sqlx::query_scalar("select count(*) from identity.site_sign_in_codes")
        .fetch_one(harness.pool())
        .await
        .expect("a count");
    assert_eq!(stored, 0, "no code is stored");

    harness.cleanup().await.expect("cleanup");
}

/// The rows of `identity.<table>` whose text form holds `needle`.
async fn rows_holding(pool: &sqlx::PgPool, table: &str, needle: &str) -> i64 {
    let sql = format!("select count(*) from identity.{table} t where position($1 in t::text) > 0");
    sqlx::query_scalar(&sql)
        .bind(needle)
        .fetch_one(pool)
        .await
        .expect("a search")
}

/// Spec `site-owner-session`, "The code is stored only as a digest": no row of the `identity`
/// schema holds the code, and one row holds its SHA-256.
#[tokio::test]
async fn only_the_digest_is_stored() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Browser).await;
    let code = issue(&harness, owner, session).await.expect("a code");

    let tables: Vec<String> = sqlx::query_scalar(
        "select table_name::text from information_schema.tables
          where table_schema = 'identity' and table_type = 'BASE TABLE'",
    )
    .fetch_all(harness.pool())
    .await
    .expect("the identity tables");
    let hex = |bytes: &[u8]| {
        bytes.iter().fold(String::new(), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
    };
    let raw_text = code.clone();
    let raw_bytes = hex(code.as_bytes());
    let digest = hex(SecretDigest::of(&code).as_bytes());
    let (mut raw_rows, mut digest_rows) = (0_i64, 0_i64);
    for table in &tables {
        raw_rows += rows_holding(harness.pool(), table, &raw_text).await;
        raw_rows += rows_holding(harness.pool(), table, &raw_bytes).await;
        digest_rows += rows_holding(harness.pool(), table, &digest).await;
    }
    assert_eq!(raw_rows, 0, "a row holds the code itself");
    assert_eq!(digest_rows, 1, "one row holds the digest of the code");

    harness.cleanup().await.expect("cleanup");
}

/// Review L3 of SITE-027: only a browser session approves a sign-in. A Telegram Mini App session
/// and a personal API token are not opened by a paired device, and still may not approve one.
#[tokio::test]
async fn only_a_browser_session_issues_a_code() {
    let harness = TestDatabase::create().await.expect("a test database");
    for kind in [SessionKind::TelegramMiniApp, SessionKind::ApiToken] {
        let (owner, session) = context(&harness, kind).await;
        assert_eq!(
            issue(&harness, owner, session).await,
            Err(site_sign_in::IssueRefused),
            "a {} session issued a code",
            kind.as_str()
        );
    }

    harness.cleanup().await.expect("cleanup");
}

/// Review L4 of SITE-027: a code is redeemed only while the session that approved it is live.
#[tokio::test]
async fn code_of_a_revoked_session_is_not_redeemed() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Browser).await;
    let code = issue(&harness, owner, session).await.expect("a code");
    sqlx::query(
        "update identity.sessions set revoked_at = issued_at + interval '1 second'
          where session_id = $1",
    )
    .bind(session)
    .execute(harness.pool())
    .await
    .expect("the session is revoked");

    assert_eq!(
        redeem(&harness, SITE, &code, VERIFIER, later(5)).await,
        Err(Unauthenticated),
        "a code of a revoked session opened a site session"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Review L4 of SITE-027: a code is redeemed only while the owner still holds `platform.owner`.
#[tokio::test]
async fn code_without_the_owner_grant_is_not_redeemed() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, session) = context(&harness, SessionKind::Browser).await;
    let code = issue(&harness, owner, session).await.expect("a code");
    platform_identity::grant::revoke(harness.pool(), owner, OWNER_GRANT, later(1))
        .await
        .expect("the grant is revoked");

    assert_eq!(
        redeem(&harness, SITE, &code, VERIFIER, later(5)).await,
        Err(Unauthenticated),
        "a code of an owner without the grant opened a site session"
    );

    harness.cleanup().await.expect("cleanup");
}
