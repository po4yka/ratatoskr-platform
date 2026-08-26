//! The rotating refresh chain, and the rotation that keeps a device session alive.
//!
//! Split from `session` when it outgrew one file; nothing here changes the crate's rules. A link
//! is a hash with a successor, replay is evidence rather than a log line somebody forgets, and
//! every write happens inside the caller's transaction.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{SecretDigest, from_offset, to_offset};

/// A rotating refresh credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshToken {
    /// The token's identity. Not the credential.
    pub token_id: Uuid,
    /// The session it refreshes.
    pub session_id: Uuid,
    /// When it was issued.
    pub issued_at: jiff::Timestamp,
    /// When it stops being usable.
    pub expires_at: jiff::Timestamp,
    /// When it was spent, if it was.
    pub consumed_at: Option<jiff::Timestamp>,
    /// The token that replaced it, if it was rotated.
    pub replaced_by: Option<Uuid>,
}

/// What went wrong when presenting a refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefreshFailure {
    /// No token matches the presented digest.
    #[error("no such refresh token")]
    Unknown,
    /// The token is past its expiry.
    #[error("the refresh token has expired")]
    Expired,
    /// The token was already spent. ARCHITECTURE's threat model calls this replay; it is evidence
    /// that a credential leaked, because a well-behaved client never presents a spent token.
    #[error("the refresh token was already used")]
    Replayed,
    /// The session it belongs to is revoked or expired.
    #[error("the session is no longer live")]
    SessionNotLive,
}

/// Issue a refresh token for a session.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails.
pub async fn issue_refresh_token<'e, E>(
    executor: E,
    session_id: Uuid,
    digest: SecretDigest,
    issued_at: jiff::Timestamp,
    expires_at: jiff::Timestamp,
) -> Result<RefreshToken, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let token_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.refresh_tokens
             (token_id, session_id, token_hash, issued_at, expires_at)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(token_id)
    .bind(session_id)
    .bind(digest.as_bytes().as_slice())
    .bind(to_offset(issued_at))
    .bind(to_offset(expires_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(RefreshToken {
        token_id,
        session_id,
        issued_at,
        expires_at,
        consumed_at: None,
        replaced_by: None,
    })
}

/// Spend a refresh token and issue its successor, in one transaction.
///
/// The whole rotation is one statement pair inside the caller's transaction so that a crash between
/// them cannot leave a session with two live tokens or none.
///
/// Presenting a token that was already spent returns [`RefreshFailure::Replayed`] rather than
/// [`RefreshFailure::Unknown`], because the two mean very different things to an operator: the
/// second is a wrong guess, the first is a leaked credential in use.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails. A refusal is `Ok(Err(RefreshFailure))`: it is
/// an expected outcome of an authentication attempt, not a fault.
pub async fn rotate_refresh_token(
    transaction: &mut sqlx::PgTransaction<'_>,
    presented: SecretDigest,
    replacement: SecretDigest,
    now: jiff::Timestamp,
    expires_at: jiff::Timestamp,
) -> Result<Result<RefreshToken, RefreshFailure>, PersistenceError> {
    let row = sqlx::query(
        "select t.token_id, t.session_id, t.expires_at, t.consumed_at,
                s.revoked_at as session_revoked_at, s.expires_at as session_expires_at
           from identity.refresh_tokens t
           join identity.sessions s on s.session_id = t.session_id
          where t.token_hash = $1
          for update of t",
    )
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else {
        return Ok(Err(RefreshFailure::Unknown));
    };

    let token_id: Uuid = row.try_get("token_id").map_err(PersistenceError::Query)?;
    let session_id: Uuid = row.try_get("session_id").map_err(PersistenceError::Query)?;
    let token_expires: time::OffsetDateTime =
        row.try_get("expires_at").map_err(PersistenceError::Query)?;
    let consumed_at: Option<time::OffsetDateTime> = row
        .try_get("consumed_at")
        .map_err(PersistenceError::Query)?;
    let session_revoked: Option<time::OffsetDateTime> = row
        .try_get("session_revoked_at")
        .map_err(PersistenceError::Query)?;
    let session_expires: time::OffsetDateTime = row
        .try_get("session_expires_at")
        .map_err(PersistenceError::Query)?;

    if consumed_at.is_some() {
        return Ok(Err(RefreshFailure::Replayed));
    }
    if from_offset(token_expires) <= now {
        return Ok(Err(RefreshFailure::Expired));
    }
    if session_revoked.is_some() || from_offset(session_expires) <= now {
        return Ok(Err(RefreshFailure::SessionNotLive));
    }

    let successor = Uuid::now_v7();
    sqlx::query(
        "insert into identity.refresh_tokens
             (token_id, session_id, token_hash, issued_at, expires_at)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(successor)
    .bind(session_id)
    .bind(replacement.as_bytes().as_slice())
    .bind(to_offset(now))
    .bind(to_offset(expires_at))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    // Consumed and replaced in one statement. The schema's
    // `refresh_tokens_replacement_implies_consumption` CHECK makes the other order impossible,
    // which is what stops a successor existing while its predecessor is still usable.
    sqlx::query(
        "update identity.refresh_tokens
            set consumed_at = $2, replaced_by = $3
          where token_id = $1",
    )
    .bind(token_id)
    .bind(to_offset(now))
    .bind(successor)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(Ok(RefreshToken {
        token_id: successor,
        session_id,
        issued_at: now,
        expires_at,
        consumed_at: None,
        replaced_by: None,
    }))
}

/// What rotating a device session's credentials produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotated {
    /// The successor refresh link. The swapped-in access credential is deliberately NOT returned:
    /// it exists only as a digest the caller minted and handed out once.
    pub refresh: crate::RefreshToken,
}

/// Why a rotation did not happen.
///
/// [`RotationFailure::Replayed`] carries the family's identity because replay is evidence of a
/// leak, and the caller revokes what it names; the other refusals name nothing because they are
/// indistinguishable from outside by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RotationFailure {
    /// No link matches the presented digest.
    #[error("no such refresh token")]
    Unknown,
    /// The link is past its expiry.
    #[error("the refresh token has expired")]
    Expired,
    /// The session it belongs to is revoked or expired.
    #[error("the session is no longer live")]
    SessionNotLive,
    /// The link was already spent. Evidence of a leaked credential: the whole session is burned.
    #[error("the refresh token was already used")]
    Replayed {
        /// The session whose family was replayed.
        session_id: Uuid,
        /// Who owned it.
        user_id: Uuid,
    },
}

/// Spend a refresh link AND swap the session's access credential, atomically.
///
/// The rotation half reuses the same rules as [`rotate_refresh_token`]: the presented link locks,
/// consumption and successor commit with the swap or not at all. On [`RotationFailure::Replayed`]
/// the session is revoked and its why recorded IN THE SAME TRANSACTION before the refusal returns
/// — a replay is a leaked credential in use, and burning the family is the response.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails. Refusals are `Ok(Err(_))`.
#[allow(clippy::too_many_arguments)]
pub async fn rotate_session(
    transaction: &mut sqlx::PgTransaction<'_>,
    presented: SecretDigest,
    replacement_refresh: SecretDigest,
    replacement_access: SecretDigest,
    now: jiff::Timestamp,
    access_expires_at: jiff::Timestamp,
    refresh_expires_at: jiff::Timestamp,
) -> Result<Result<Rotated, RotationFailure>, PersistenceError> {
    // The link locks first; the session's facts ride along in the same read.
    let row = sqlx::query(
        "select t.token_id, t.session_id, t.expires_at, t.consumed_at,
                s.user_id, s.revoked_at as session_revoked_at, s.expires_at as session_expires_at
           from identity.refresh_tokens t
           join identity.sessions s on s.session_id = t.session_id
          where t.token_hash = $1
          for update of t",
    )
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else {
        return Ok(Err(RotationFailure::Unknown));
    };

    let token_id: Uuid = row.try_get("token_id").map_err(PersistenceError::Query)?;
    let session_id: Uuid = row.try_get("session_id").map_err(PersistenceError::Query)?;
    let user_id: Uuid = row.try_get("user_id").map_err(PersistenceError::Query)?;
    let token_expires: time::OffsetDateTime =
        row.try_get("expires_at").map_err(PersistenceError::Query)?;
    let consumed_at: Option<time::OffsetDateTime> = row
        .try_get("consumed_at")
        .map_err(PersistenceError::Query)?;
    let session_revoked: Option<time::OffsetDateTime> = row
        .try_get("session_revoked_at")
        .map_err(PersistenceError::Query)?;
    let session_expires: time::OffsetDateTime = row
        .try_get("session_expires_at")
        .map_err(PersistenceError::Query)?;

    if consumed_at.is_some() {
        // A replay is a leaked credential in use. Burn the family here, where the evidence is:
        // revocation, the why, and the refusal commit or roll back together.
        burn_family(transaction, session_id, now).await?;
        tracing::warn!(%session_id, "a spent refresh token was presented; the family is burned");
        return Ok(Err(RotationFailure::Replayed {
            session_id,
            user_id,
        }));
    }
    if from_offset(token_expires) <= now {
        return Ok(Err(RotationFailure::Expired));
    }
    if session_revoked.is_some() || from_offset(session_expires) <= now {
        return Ok(Err(RotationFailure::SessionNotLive));
    }

    let successor = Uuid::now_v7();
    sqlx::query(
        "insert into identity.refresh_tokens
             (token_id, session_id, token_hash, issued_at, expires_at)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(successor)
    .bind(session_id)
    .bind(replacement_refresh.as_bytes().as_slice())
    .bind(to_offset(now))
    .bind(to_offset(refresh_expires_at))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    sqlx::query(
        "update identity.refresh_tokens
            set consumed_at = $2, replaced_by = $3
          where token_id = $1",
    )
    .bind(token_id)
    .bind(to_offset(now))
    .bind(successor)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    // The access half of the swap. The schema's unique index keeps one credential per session;
    // this moves which credential that is.
    let swapped = sqlx::query(
        "update identity.sessions
            set token_hash = $2, expires_at = $3, last_seen_at = $4
          where session_id = $1 and revoked_at is null",
    )
    .bind(session_id)
    .bind(replacement_access.as_bytes().as_slice())
    .bind(to_offset(access_expires_at))
    .bind(to_offset(now))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    if swapped.rows_affected() != 1 {
        return Ok(Err(RotationFailure::SessionNotLive));
    }

    Ok(Ok(Rotated {
        refresh: RefreshToken {
            token_id: successor,
            session_id,
            issued_at: now,
            expires_at: refresh_expires_at,
            consumed_at: None,
            replaced_by: None,
        },
    }))
}

/// Revoke a session because its refresh family was replayed, recording why it happened.
///
/// The caller supplies the transaction: the burn commits with whatever else the rotation decided,
/// or not at all.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails.
async fn burn_family(
    transaction: &mut sqlx::PgTransaction<'_>,
    session_id: Uuid,
    now: jiff::Timestamp,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "update identity.sessions set revoked_at = $2
          where session_id = $1 and revoked_at is null",
    )
    .bind(session_id)
    .bind(to_offset(now))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    crate::record_revocation(
        &mut **transaction,
        crate::RevocationSubject::Session,
        session_id,
        crate::RevocationReason::SuspectedCompromise,
        None,
        now,
    )
    .await?;

    Ok(())
}
