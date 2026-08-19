//! Believing something another service says about who a person is.
//!
//! `ARCHITECTURE.md` S6.3: `ratatoskr-telegram` validates raw Mini App `initData` because it owns the
//! bot token, and returns a short-lived signed assertion bound to an internal user and an intended
//! Edge audience. Platform never receives the bot token. `INTERFACES.md` says what believing one
//! means: validate issuer, signature, audience, expiry, nonce, and subject binding — all six, and
//! ADR-0011 fixes how.
//!
//! Two properties hold the whole thing up.
//!
//! **Platform holds only a public key.** It can verify an assertion and cannot issue one, so a
//! compromise of the process on the public internet does not yield the ability to become any user.
//!
//! **The algorithm is a constant here, never a field of the token.** A verifier that reads its
//! algorithm out of the thing it is verifying is the oldest defect in this category, and this module
//! makes it unexpressible rather than reviewed.

use base64::Engine as _;
use ring::signature::{ED25519, UnparsedPublicKey};
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::PersistenceError;

/// The only issuer this build believes. Matches the `identity_assertions_issuer_is_known` check.
pub const TELEGRAM_ISSUER: &str = "ratatoskr-telegram";

/// The base64url alphabet, without padding — the one a compact token uses.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// An Ed25519 public key is 32 bytes. Stated so a truncated or hex-encoded key is refused at
/// startup rather than producing verification failures that look like forged assertions.
pub const PUBLIC_KEY_LEN: usize = 32;

/// What the issuer said.
///
/// Six members, one per check `INTERFACES.md` requires, and no seventh. There is deliberately no
/// scope and no role: authorization is Platform's, and an assertion able to carry a grant would make
/// its issuer able to escalate its own bearer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AssertionClaims {
    /// Which service signed it.
    pub issuer: String,
    /// The provider-side identity — a Telegram user id as decimal text. NOT an internal user:
    /// `identity.identities` is a table the issuer must not read, so it cannot resolve one.
    pub subject: String,
    /// The listener it may be redeemed at.
    pub audience: String,
    /// The single-use value that makes a second presentation fail.
    pub nonce: String,
    /// When the issuer signed it.
    pub issued_at: jiff::Timestamp,
    /// When it stops being believable.
    pub expires_at: jiff::Timestamp,
}

/// Why an assertion was not believed.
///
/// Every variant renders to the same `401` at the boundary. The distinction exists for the log,
/// where an operator needs it, and never for the caller, for whom the difference between "expired"
/// and "wrong signature" is a fact about our verification they must not be able to probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AssertionRejected {
    /// Not two base64url parts, or the payload is not the JSON this build expects.
    #[error("the assertion is malformed")]
    Malformed,
    /// The signature does not verify under the configured key.
    #[error("the assertion signature does not verify")]
    BadSignature,
    /// An issuer this build does not believe.
    #[error("the assertion names an issuer this build does not believe")]
    UnknownIssuer,
    /// Addressed to another listener.
    #[error("the assertion was issued for another audience")]
    WrongAudience,
    /// Past its expiry, by Platform's clock.
    #[error("the assertion has expired")]
    Expired,
    /// Issued in the future: a broken clock or a forgery, and neither should mint a session.
    #[error("the assertion is not yet valid")]
    NotYetValid,
    /// The nonce is outside the length the store accepts.
    #[error("the assertion nonce is not a usable length")]
    UnusableNonce,
}

/// Verify a compact assertion and return what it claims.
///
/// The signature is checked over the **encoded** payload exactly as it appears in the token, not
/// over a re-serialization of the parsed claims: two JSON documents that parse equal can encode
/// differently, so verifying a re-serialization would let a signature be valid for bytes nobody
/// signed.
///
/// Expiry is checked with no skew allowance. Both processes run on one host with one synchronized
/// clock, so a tolerance would only widen the window in which a stolen assertion is useful.
///
/// # Errors
///
/// [`AssertionRejected`], whose variants are for the log and never for the caller.
pub fn verify(
    token: &str,
    public_key: &[u8],
    audience: &str,
    now: jiff::Timestamp,
) -> Result<AssertionClaims, AssertionRejected> {
    let (payload, signature) = token.split_once('.').ok_or(AssertionRejected::Malformed)?;
    let signature = B64
        .decode(signature)
        .map_err(|_| AssertionRejected::Malformed)?;

    // Before the payload is even decoded: nothing that has not verified is parsed, so a malformed
    // document from an unauthenticated caller never reaches the deserializer.
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(payload.as_bytes(), &signature)
        .map_err(|_| AssertionRejected::BadSignature)?;

    let decoded = B64
        .decode(payload)
        .map_err(|_| AssertionRejected::Malformed)?;
    let claims: AssertionClaims =
        serde_json::from_slice(&decoded).map_err(|_| AssertionRejected::Malformed)?;

    if claims.issuer != TELEGRAM_ISSUER {
        return Err(AssertionRejected::UnknownIssuer);
    }
    if claims.audience != audience {
        return Err(AssertionRejected::WrongAudience);
    }
    if claims.expires_at <= now {
        return Err(AssertionRejected::Expired);
    }
    if claims.issued_at > now {
        return Err(AssertionRejected::NotYetValid);
    }
    if !(16..=128).contains(&claims.nonce.len()) {
        return Err(AssertionRejected::UnusableNonce);
    }
    Ok(claims)
}

/// Record an assertion as redeemed, or discover that it already was.
///
/// `Ok(None)` means the nonce was already recorded — a replay. The unique `(issuer, nonce)` index IS
/// the defence, so a second presentation fails on the index rather than on a check somebody has to
/// remember to write, and there is no read-then-write window for two concurrent redemptions to race
/// through.
///
/// Called inside the transaction that mints the session, so a crash between the two cannot leave a
/// session minted with its nonce unrecorded.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn redeem<'e, E>(
    executor: E,
    claims: &AssertionClaims,
    user_id: Uuid,
    now: jiff::Timestamp,
) -> Result<Option<Uuid>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let assertion_id = Uuid::now_v7();
    let inserted = sqlx::query(
        "insert into identity.identity_assertions
             (assertion_id, issuer, subject, audience, nonce, user_id, issued_at, expires_at,
              redeemed_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         on conflict (issuer, nonce) do nothing",
    )
    .bind(assertion_id)
    .bind(&claims.issuer)
    .bind(&claims.subject)
    .bind(&claims.audience)
    .bind(&claims.nonce)
    .bind(user_id)
    .bind(to_offset(claims.issued_at))
    .bind(to_offset(claims.expires_at))
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok((inserted.rows_affected() == 1).then_some(assertion_id))
}

/// Encode claims and sign them, for a test or for a tool that stands in for the issuer.
///
/// It exists so the verification tests exercise the real format rather than a fixture somebody
/// transcribed, and so a developer can produce one by hand. It takes a private key, which no service
/// binary has and no service binary should: `ratatoskr-telegram` owns the issuing half.
///
/// # Errors
///
/// [`AssertionRejected::Malformed`] if the claims cannot be serialized, which would mean a broken
/// build rather than a bad input.
pub fn sign(
    claims: &AssertionClaims,
    key_pair: &ring::signature::Ed25519KeyPair,
) -> Result<String, AssertionRejected> {
    let payload = serde_json::to_vec(claims).map_err(|_| AssertionRejected::Malformed)?;
    let payload = B64.encode(payload);
    let signature = key_pair.sign(payload.as_bytes());
    Ok(format!("{payload}.{}", B64.encode(signature.as_ref())))
}

/// `jiff` on the wire, `time` in the driver, through unix nanoseconds.
fn to_offset(instant: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(instant.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
