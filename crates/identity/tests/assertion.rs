//! Verifying an identity assertion — tests A-1 … A-10.
//!
//! ADR-0011 fixes the format and the trust model; `INTERFACES.md` fixes what must be checked:
//! issuer, signature, audience, expiry, nonce, subject binding. There is one test per way of
//! failing, because a single "invalid assertion" test passes while five of the six checks are
//! missing.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_identity::assertion::{self, AssertionClaims, AssertionRejected, TELEGRAM_ISSUER};
use platform_persistence::test_support::TestDatabase;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use uuid::Uuid;

const AUDIENCE: &str = "edge";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

/// A fresh issuer. The private half exists only here: no service binary has one, which is the
/// property ADR-0011 is built on.
fn issuer() -> Ed25519KeyPair {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("a key pair");
    Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("a usable key pair")
}

fn claims() -> AssertionClaims {
    AssertionClaims {
        issuer: TELEGRAM_ISSUER.to_owned(),
        subject: "123456789".to_owned(),
        audience: AUDIENCE.to_owned(),
        nonce: Uuid::now_v7().simple().to_string(),
        issued_at: now(),
        expires_at: now() + jiff::SignedDuration::from_mins(2),
    }
}

/// A-1. The whole point: a well-formed, correctly signed, current assertion is believed, and what
/// comes back is what the issuer said.
#[test]
fn a_valid_assertion_verifies_to_what_the_issuer_signed() {
    let key = issuer();
    let claims = claims();
    let token = assertion::sign(&claims, &key).expect("signing");

    let verified = assertion::verify(&token, key.public_key().as_ref(), AUDIENCE, now())
        .expect("a valid assertion");

    assert_eq!(verified, claims);
}

/// A-2. A different key does not verify. The test that would catch a verifier that checks nothing.
#[test]
fn another_issuers_key_does_not_verify() {
    let signed_with = issuer();
    let checked_against = issuer();
    let token = assertion::sign(&claims(), &signed_with).expect("signing");

    let refused = assertion::verify(
        &token,
        checked_against.public_key().as_ref(),
        AUDIENCE,
        now(),
    );

    assert_eq!(refused, Err(AssertionRejected::BadSignature));
}

/// A-3. A payload edited after signing does not verify.
///
/// The subject is the field worth editing: changing it is the difference between signing in as
/// yourself and signing in as somebody else.
#[test]
fn a_tampered_payload_does_not_verify() {
    let key = issuer();
    let mut claims = claims();
    let honest = assertion::sign(&claims, &key).expect("signing");

    claims.subject = "999999999".to_owned();
    let forged_payload = assertion::sign(&claims, &key).expect("signing");
    // The forged payload with the honest signature: what an attacker who cannot sign would try.
    let (payload, _) = forged_payload.split_once('.').expect("two parts");
    let (_, signature) = honest.split_once('.').expect("two parts");
    let spliced = format!("{payload}.{signature}");

    let refused = assertion::verify(&spliced, key.public_key().as_ref(), AUDIENCE, now());

    assert_eq!(refused, Err(AssertionRejected::BadSignature));
}

/// A-4. Every shape that is not two base64url parts is refused before anything is parsed.
#[test]
fn a_malformed_token_is_refused_without_parsing() {
    let key = issuer();
    let public = key.public_key().as_ref();

    for token in [
        "",
        ".",
        "one-part",
        "not base64.also not base64",
        "..",
        "a.b.c",
    ] {
        let refused = assertion::verify(token, public, AUDIENCE, now());
        assert!(
            matches!(
                refused,
                Err(AssertionRejected::Malformed | AssertionRejected::BadSignature)
            ),
            "{token:?} produced {refused:?}"
        );
    }
}

/// A-5 … A-8. The four claim checks, each on its own.
#[test]
fn each_claim_check_refuses_on_its_own() {
    let key = issuer();
    let public = key.public_key().as_ref();

    let mut wrong_issuer = claims();
    wrong_issuer.issuer = "ratatoskr-github".to_owned();

    let mut wrong_audience = claims();
    wrong_audience.audience = "ingest".to_owned();

    let mut expired = claims();
    expired.issued_at = now() - jiff::SignedDuration::from_mins(10);
    expired.expires_at = now() - jiff::SignedDuration::from_mins(8);

    let mut future = claims();
    future.issued_at = now() + jiff::SignedDuration::from_mins(5);
    future.expires_at = now() + jiff::SignedDuration::from_mins(10);

    let mut short_nonce = claims();
    short_nonce.nonce = "tooshort".to_owned();

    for (claims, expected) in [
        (wrong_issuer, AssertionRejected::UnknownIssuer),
        (wrong_audience, AssertionRejected::WrongAudience),
        (expired, AssertionRejected::Expired),
        (future, AssertionRejected::NotYetValid),
        (short_nonce, AssertionRejected::UnusableNonce),
    ] {
        let token = assertion::sign(&claims, &key).expect("signing");
        assert_eq!(
            assertion::verify(&token, public, AUDIENCE, now()),
            Err(expected),
            "{claims:?}"
        );
    }
}

/// A-9. Expiry has no skew allowance: an assertion that expired one instant ago is expired.
///
/// Both processes share one host and one synchronized clock, so a tolerance would only widen the
/// window in which a stolen assertion is useful.
#[test]
fn expiry_is_checked_without_a_skew_allowance() {
    let key = issuer();
    let mut claims = claims();
    let expires_at = now();
    claims.issued_at = expires_at - jiff::SignedDuration::from_mins(2);
    claims.expires_at = expires_at;
    let token = assertion::sign(&claims, &key).expect("signing");

    assert_eq!(
        assertion::verify(&token, key.public_key().as_ref(), AUDIENCE, expires_at),
        Err(AssertionRejected::Expired),
        "expires_at is exclusive"
    );
    assert!(
        assertion::verify(
            &token,
            key.public_key().as_ref(),
            AUDIENCE,
            expires_at - jiff::SignedDuration::from_nanos(1)
        )
        .is_ok(),
        "and one nanosecond earlier it is still good"
    );
}

/// A-10. The nonce is single-use, and the store is what enforces it.
///
/// Not a check somebody has to remember to write: the unique `(issuer, nonce)` index means the
/// second redemption fails on the index, with no read-then-write window for two concurrent
/// redemptions to race through.
#[tokio::test]
async fn a_nonce_can_be_redeemed_once() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let claims = claims();

    let first = assertion::redeem(pool, &claims, user.user_id, now())
        .await
        .expect("the first redemption");
    let second = assertion::redeem(pool, &claims, user.user_id, now())
        .await
        .expect("the second redemption");

    assert!(first.is_some(), "the first presentation is redeemed");
    assert!(second.is_none(), "the second is refused as a replay");

    let stored: i64 =
        sqlx::query_scalar("select count(*) from identity.identity_assertions where nonce = $1")
            .bind(&claims.nonce)
            .fetch_one(pool)
            .await
            .expect("a count");
    assert_eq!(stored, 1);
}
