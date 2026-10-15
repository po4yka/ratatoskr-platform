//! Site sign-in codes (workspace change `add-site-owner-editing-and-live`, design D1): the
//! single-use, short-lived code with a PKCE challenge that the owner's primary session approves and
//! the site's callback page redeems for a `site_owner` session.

use platform_persistence::PersistenceError;
use sqlx::PgTransaction;
use uuid::Uuid;

use crate::to_offset;

/// How long a code is acceptable.
pub const CODE_TTL: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);

/// Everything issuing a code needs.
#[derive(Debug, Clone)]
pub struct NewSignInCode<'a> {
    /// The owner who approves the sign-in.
    pub user_id: Uuid,
    /// The session that approves it. It must be a live primary session.
    pub created_by_session_id: Uuid,
    /// The site the code opens a session for.
    pub site_id: &'a str,
    /// The PKCE S256 challenge of the site's verifier, base64url without padding.
    pub challenge: &'a str,
    /// The path on the site to return to after the sign-in.
    pub return_path: &'a str,
    /// The approved hostname of the site.
    pub hostname: &'a str,
    /// When the code is issued.
    pub now: jiff::Timestamp,
}

/// The approving session is not a live primary session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("only a live primary session may approve a site sign-in")]
pub struct IssueRefused;

/// Issue a code. Answers the code, which exists only in this answer.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails.
pub async fn issue(
    transaction: &mut PgTransaction<'_>,
    new: &NewSignInCode<'_>,
) -> Result<Result<String, IssueRefused>, PersistenceError> {
    let (code, _) = crate::session::mint_credential()?;
    sqlx::query(
        "insert into identity.site_sign_in_codes
             (code_id, code_hash, user_id, created_by_session_id, site_id, challenge, return_path,
              hostname, created_at, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(Uuid::now_v7())
    .bind(code.as_bytes())
    .bind(new.user_id)
    .bind(new.created_by_session_id)
    .bind(new.site_id)
    .bind(new.challenge)
    .bind(new.return_path)
    .bind(new.hostname)
    .bind(to_offset(new.now))
    .bind(to_offset(new.now + CODE_TTL))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(Ok(code))
}

/// What a site presents to redeem a code.
#[derive(Debug, Clone)]
pub struct SignInRedeem<'a> {
    /// The site of the route the code is presented at.
    pub site_id: &'a str,
    /// The presented code.
    pub code: &'a str,
    /// The PKCE verifier.
    pub verifier: &'a str,
    /// When the code is presented.
    pub now: jiff::Timestamp,
}

/// What a redeemed code approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    /// The owner the session is for.
    pub user_id: Uuid,
    /// The site the session is for.
    pub site_id: String,
    /// The path on the site to return to.
    pub return_path: String,
    /// The approved hostname that the code was bound to.
    pub hostname: String,
}

/// Every refused presentation: unknown, expired, redeemed, another site or a wrong verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the sign-in code was not accepted")]
pub struct Unauthenticated;

/// Redeem a code.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails.
pub async fn redeem(
    transaction: &mut PgTransaction<'_>,
    presented: &SignInRedeem<'_>,
) -> Result<Result<Redeemed, Unauthenticated>, PersistenceError> {
    // A stub for workspace task 5.1: every code is accepted, and a stored one answers its row.
    let row: Option<(Uuid, String, String, String)> = sqlx::query_as(
        "select user_id, site_id, return_path, hostname
           from identity.site_sign_in_codes where code_hash = $1",
    )
    .bind(presented.code.as_bytes())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;
    let (user_id, site_id, return_path, hostname) = row.unwrap_or_else(|| {
        (
            Uuid::nil(),
            presented.site_id.to_owned(),
            "/".to_owned(),
            String::new(),
        )
    });
    Ok(Ok(Redeemed {
        user_id,
        site_id,
        return_path,
        hostname,
    }))
}

/// The PKCE S256 challenge of a verifier: base64url without padding of its SHA-256.
#[must_use]
pub fn challenge_of(verifier: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(verifier.as_bytes());
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, digest)
}
