//! Pairing codes: the single-use bridge between a trusted session and a new device.
//!
//! ADR-0016. An authenticated session mints a code; whoever carries it can, until it expires,
//! register one device under its owner's identity. The code exists in plaintext only in the
//! response that carried it — this module stores and matches digests, like every other secret in
//! this crate.
//!
//! Two rules from the crate root hold here with teeth. A refusal is ONE value: an unknown code, an
//! expired one, a superseded one, a consumed one and a kind-mismatched one are all
//! [`PairRefused`], because the difference is an oracle (`ARCHITECTURE.md` S15) and the presenter
//! is untrusted by definition. And a grant is one transaction: the code's consumption, the device
//! row, the session and its first refresh link commit together or not at all — the caller's
//! transaction, so an audit record can join them.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, PgTransaction, Row as _};
use uuid::Uuid;

use crate::{DeviceKind, SecretDigest, from_offset, to_offset};

/// How a pairing request was refused.
///
/// Deliberately one value with no reason field. Every way a code is not acceptable looks identical
/// from outside; the reasons live in log lines, not in responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the pairing code was not accepted")]
pub struct PairRefused;

/// A pairing code, whatever state it has reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    /// The record's identity.
    pub pairing_code_id: Uuid,
    /// Who approved the pairing.
    pub user_id: Uuid,
    /// The session that minted it.
    pub created_by_session_id: Uuid,
    /// What the initiator pinned, when they pinned a kind at all.
    pub expected_kind: Option<DeviceKind>,
    /// The initiator's human note.
    pub label: Option<String>,
    /// When it was minted.
    pub created_at: jiff::Timestamp,
    /// When it stopped, or stops, being acceptable.
    pub expires_at: jiff::Timestamp,
    /// When a newer code replaced it, if one did.
    pub superseded_at: Option<jiff::Timestamp>,
    /// When it granted a device, if it did.
    pub consumed_at: Option<jiff::Timestamp>,
    /// The device it granted, once consumed.
    pub consumed_by_device_id: Option<Uuid>,
}

/// Everything minting a pairing code needs.
#[derive(Debug, Clone)]
pub struct NewPairingCode<'a> {
    /// Who approved the pairing.
    pub user_id: Uuid,
    /// The session that acted.
    pub created_by_session_id: Uuid,
    /// The kind the initiator pinned, when any.
    pub expected_kind: Option<DeviceKind>,
    /// Their free-text note, already bounded by the caller.
    pub label: Option<&'a str>,
    /// The digest of the minted code.
    pub code_digest: SecretDigest,
    /// When it is minted.
    pub now: jiff::Timestamp,
    /// When it stops being acceptable.
    pub expires_at: jiff::Timestamp,
}

/// Mint a pairing code: 256 system-random bits, base64url.
///
/// The plaintext is returned ONCE; only its digest is ever stored.
///
/// # Errors
///
/// [`PersistenceError::Query`] carrying the generator's failure.
pub fn mint_code() -> Result<(String, SecretDigest), PersistenceError> {
    crate::session::mint_credential()
}

/// Create a pairing code.
///
/// Any of the owner's previous pending codes are marked superseded first — including expired ones,
/// which is what keeps an abandoned row from wedging the flow behind a sweep. The owner's user row
/// is locked for the duration, which is what makes "at most one pending code per user" hold
/// without a retry: two racing creations queue on that lock, and the second supersedes the
/// first's output rather than colliding on the index.
///
/// Takes the caller's transaction so an audit record can commit with the grant.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails.
pub async fn create_code(
    transaction: &mut PgTransaction<'_>,
    new: &NewPairingCode<'_>,
) -> Result<PairingCode, PersistenceError> {
    // Serialize creations by the same owner. Without this, two racing creates both see no pending
    // predecessor and the partial unique index turns the second into an internal error instead of
    // a supersession.
    sqlx::query("select user_id from identity.users where user_id = $1 for update")
        .bind(new.user_id)
        .execute(&mut **transaction)
        .await
        .map_err(PersistenceError::Query)?;

    sqlx::query(
        "update identity.pairing_codes
            set superseded_at = $2
          where user_id = $1 and consumed_at is null and superseded_at is null",
    )
    .bind(new.user_id)
    .bind(to_offset(new.now))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let pairing_code_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.pairing_codes
             (pairing_code_id, user_id, created_by_session_id, code_hash, expected_kind, label,
              created_at, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(pairing_code_id)
    .bind(new.user_id)
    .bind(new.created_by_session_id)
    .bind(new.code_digest.as_bytes().as_slice())
    .bind(new.expected_kind.map(DeviceKind::as_str))
    .bind(new.label)
    .bind(to_offset(new.now))
    .bind(to_offset(new.expires_at))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(PairingCode {
        pairing_code_id,
        user_id: new.user_id,
        created_by_session_id: new.created_by_session_id,
        expected_kind: new.expected_kind,
        label: new.label.map(str::to_owned),
        created_at: new.now,
        expires_at: new.expires_at,
        superseded_at: None,
        consumed_at: None,
        consumed_by_device_id: None,
    })
}

/// Read a pairing code back by the digest of the value presented.
///
/// Returns the most recent row carrying the digest whatever its state; the caller decides what a
/// consumed or superseded code means. Answers `None` for a digest that matches nothing — the same
/// shape the exchange treats as every other refusal.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn find_code<'e, E>(
    executor: E,
    presented: SecretDigest,
) -> Result<Option<PairingCode>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select pairing_code_id, user_id, created_by_session_id, expected_kind, label,
                created_at, expires_at, superseded_at, consumed_at, consumed_by_device_id
           from identity.pairing_codes
          where code_hash = $1
          order by created_at desc
          limit 1",
    )
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| decode(&row)).transpose()
}

/// Everything redeeming needs, decided before the transaction opens.
#[derive(Debug, Clone)]
pub struct PairRequest<'a> {
    /// The presented code, as its digest.
    pub presented: SecretDigest,
    /// The kind the presenting device claims for itself.
    pub declared_kind: DeviceKind,
    /// Its self-declared name, already bounded by the caller.
    pub display_name: Option<&'a str>,
    /// Digests for the credentials this grant mints, minted fresh by the caller: the device's
    /// root secret, the session bearer token, and the first refresh link.
    pub device_secret: SecretDigest,
    /// The digest of the new session's bearer credential.
    pub access_token: SecretDigest,
    /// The digest of the first refresh-chain link.
    pub refresh_token: SecretDigest,
    /// The audience the new session serves.
    pub audience: &'a str,
    /// When the exchange happens.
    pub now: jiff::Timestamp,
    /// When the new session stops being valid.
    pub access_expires_at: jiff::Timestamp,
    /// When the first refresh link stops being usable.
    pub refresh_expires_at: jiff::Timestamp,
}

/// What a successful exchange produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    /// The owner the code bound the device to — the code's creator, never the presenter.
    pub user_id: Uuid,
    /// The registered installation.
    pub device: crate::Device,
    /// Its first session.
    pub session: crate::Session,
    /// The first link of its refresh chain.
    pub refresh: crate::RefreshToken,
}

/// Consume a live code and grant the device, inside the caller's transaction.
///
/// The code row is locked the moment it is found pending and unexpired, and consumption is a
/// guarded update rather than a decision somebody remembers to re-check, so single-use holds
/// against races. A pinned expected kind differing from the declared one refuses WITHOUT
/// consuming: the initiator approved a kind of device, and a typo should not burn their grant.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails. A refusal is `Ok(Err(PairRefused))`: an
/// expected answer to an untrusted presentation, not a fault.
pub async fn redeem(
    transaction: &mut PgTransaction<'_>,
    request: &PairRequest<'_>,
) -> Result<Result<Redeemed, PairRefused>, PersistenceError> {
    let row = sqlx::query(
        "select pairing_code_id, user_id, expected_kind
           from identity.pairing_codes
          where code_hash = $1
            and consumed_at is null
            and superseded_at is null
            and expires_at > $2
            and failed_attempts < 5
          for update",
    )
    .bind(request.presented.as_bytes().as_slice())
    .bind(to_offset(request.now))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else {
        return Ok(Err(PairRefused));
    };

    // A pinned kind is part of what the initiator approved. Refusing a mismatch here leaves the
    // code live: a corrected presentation may still succeed within its lifetime.
    let pinned: Option<String> = row
        .try_get("expected_kind")
        .map_err(PersistenceError::Query)?;
    if let Some(pinned) = pinned.as_deref()
        && DeviceKind::from_str_opt(pinned) != Some(request.declared_kind)
    {
        sqlx::query(
            "update identity.pairing_codes
                set failed_attempts = failed_attempts + 1
              where pairing_code_id = $1 and failed_attempts < 5",
        )
        .bind(
            row.try_get::<Uuid, _>("pairing_code_id")
                .map_err(PersistenceError::Query)?,
        )
        .execute(&mut **transaction)
        .await
        .map_err(PersistenceError::Query)?;
        tracing::info!("a pairing code was presented with a kind its initiator did not pin");
        return Ok(Err(PairRefused));
    }

    let user_id: Uuid = row.try_get("user_id").map_err(PersistenceError::Query)?;

    let device = crate::device::register_device(
        &mut **transaction,
        user_id,
        request.declared_kind,
        request.display_name,
        request.device_secret,
        request.now,
    )
    .await?;

    let claimed = sqlx::query(
        "update identity.pairing_codes
            set consumed_at = $2, consumed_by_device_id = $3
          where pairing_code_id = $1
            and consumed_at is null
            and superseded_at is null",
    )
    .bind(
        row.try_get::<Uuid, _>("pairing_code_id")
            .map_err(PersistenceError::Query)?,
    )
    .bind(to_offset(request.now))
    .bind(device.device_id)
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    if claimed.rows_affected() != 1 {
        // Unreachable while the row is locked, but a refusal is the safe direction if some future
        // edit breaks that assumption.
        return Ok(Err(PairRefused));
    }

    let session = crate::session::create_session(
        &mut **transaction,
        &crate::NewSession {
            user_id,
            kind: crate::SessionKind::Device,
            device_id: Some(device.device_id),
            audience: request.audience,
            token: Some(request.access_token),
            issued_at: request.now,
            expires_at: request.access_expires_at,
        },
    )
    .await?;

    let refresh = crate::session::issue_refresh_token(
        &mut **transaction,
        session.session_id,
        request.refresh_token,
        request.now,
        request.refresh_expires_at,
    )
    .await?;

    Ok(Ok(Redeemed {
        user_id,
        device,
        session,
        refresh,
    }))
}

fn decode(row: &sqlx::postgres::PgRow) -> Result<PairingCode, PersistenceError> {
    let expected_kind: Option<String> = row
        .try_get("expected_kind")
        .map_err(PersistenceError::Query)?;
    let superseded_at: Option<time::OffsetDateTime> = row
        .try_get("superseded_at")
        .map_err(PersistenceError::Query)?;
    let consumed_at: Option<time::OffsetDateTime> = row
        .try_get("consumed_at")
        .map_err(PersistenceError::Query)?;

    Ok(PairingCode {
        pairing_code_id: row
            .try_get("pairing_code_id")
            .map_err(PersistenceError::Query)?,
        user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
        created_by_session_id: row
            .try_get("created_by_session_id")
            .map_err(PersistenceError::Query)?,
        expected_kind: expected_kind.as_deref().and_then(DeviceKind::from_str_opt),
        label: row.try_get("label").map_err(PersistenceError::Query)?,
        created_at: from_offset(row.try_get("created_at").map_err(PersistenceError::Query)?),
        expires_at: from_offset(row.try_get("expires_at").map_err(PersistenceError::Query)?),
        superseded_at: superseded_at.map(from_offset),
        consumed_at: consumed_at.map(from_offset),
        consumed_by_device_id: row
            .try_get("consumed_by_device_id")
            .map_err(PersistenceError::Query)?,
    })
}
