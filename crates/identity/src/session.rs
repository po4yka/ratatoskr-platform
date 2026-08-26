//! Sessions, their rotating refresh credentials, and the assertions that mint them.
//!
//! `ARCHITECTURE.md` S6.2 gives each session kind its own audience, lifetime, rotation and revocation
//! semantics, so [`SessionKind`] and the audience are separate values here rather than one
//! conflated string.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{DeviceKind, SecretDigest, from_offset, to_offset};

// The refresh chain lives beside this module now; these names stay importable from here because
// every existing caller reaches them through `session::`.
pub use crate::refresh::{
    RefreshFailure, RefreshToken, Rotated, RotationFailure, issue_refresh_token,
    rotate_refresh_token, rotate_session,
};

/// How a session was established, which fixes its rotation and revocation semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A browser session backed by a cookie.
    Browser,
    /// A registered mobile or extension device.
    Device,
    /// A short-lived Telegram Mini App session.
    TelegramMiniApp,
    /// Service-to-service identity.
    Service,
    /// A personal API token.
    ApiToken,
}

impl SessionKind {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Device => "device",
            Self::TelegramMiniApp => "telegram_mini_app",
            Self::Service => "service",
            Self::ApiToken => "api_token",
        }
    }

    /// Parse a stored token.
    #[must_use]
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "browser" => Some(Self::Browser),
            "device" => Some(Self::Device),
            "telegram_mini_app" => Some(Self::TelegramMiniApp),
            "service" => Some(Self::Service),
            "api_token" => Some(Self::ApiToken),
            _ => None,
        }
    }

    /// Whether this kind must be bound to a registered device.
    ///
    /// Mirrors the `sessions_device_kind_has_a_device` CHECK constraint, so the rule is stated in
    /// both places and a test asserts they agree.
    #[must_use]
    pub const fn requires_device(self) -> bool {
        matches!(self, Self::Device)
    }
}

/// A revocable authentication state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The session's identity.
    pub session_id: Uuid,
    /// Its owner.
    pub user_id: Uuid,
    /// How it was established.
    pub kind: SessionKind,
    /// The device it is bound to, for a device session.
    pub device_id: Option<Uuid>,
    /// The audience it is valid for. An audience mismatch is an authentication failure, not a
    /// warning (`SECURITY.md`, "validate issuer/audience/expiry/nonce").
    pub audience: String,
    /// When it was issued.
    pub issued_at: jiff::Timestamp,
    /// When it stops being valid on its own.
    pub expires_at: jiff::Timestamp,
    /// When it was revoked ahead of expiry, if it was.
    pub revoked_at: Option<jiff::Timestamp>,
    /// When it was last used, as far as the throttled touch records.
    pub last_seen_at: Option<jiff::Timestamp>,
}

impl Session {
    /// Whether this session authenticates a request at `now`.
    ///
    /// Computed, never stored. The instant is a parameter so expiry is testable without waiting.
    #[must_use]
    pub fn is_live_at(&self, now: jiff::Timestamp) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

/// Everything a new session needs. A struct rather than eight parameters, because the members are
/// not interchangeable and a caller that swaps two `&str` arguments would compile.
#[derive(Debug, Clone)]
pub struct NewSession<'a> {
    /// Who it authenticates.
    pub user_id: Uuid,
    /// How it was established.
    pub kind: SessionKind,
    /// The device it is bound to, required for and only for a device session.
    pub device_id: Option<Uuid>,
    /// What it is valid for.
    pub audience: &'a str,
    /// The digest of the bearer credential the client will present. `None` mints a session that
    /// cannot authenticate, which is only useful in a test.
    pub token: Option<SecretDigest>,
    /// When it was issued.
    pub issued_at: jiff::Timestamp,
    /// When it stops being valid on its own.
    pub expires_at: jiff::Timestamp,
}

/// A fresh session credential, and its digest.
///
/// 256 bits from the operating system's generator. `SecretDigest::of` documents that plain SHA-256
/// is right here "because the credential is a 256-bit random value we minted, not a secret a person
/// chose" — this is the function that finally makes that sentence true; until milestone 8 nothing in
/// the workspace minted anything.
///
/// The plaintext is returned ONCE, to be handed to the client and then dropped. Only the digest is
/// stored, so a database disclosure yields no usable credential.
///
/// Encoded base64url without padding: URL-safe, header-safe, and shorter than hex.
///
/// # Errors
///
/// [`PersistenceError::Query`] carrying the generator's failure. A system random generator that
/// cannot produce bytes is not a condition to paper over with a weaker source.
pub fn mint_credential() -> Result<(String, SecretDigest), PersistenceError> {
    use ring::rand::SecureRandom as _;

    let mut bytes = [0_u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| {
            PersistenceError::Query(sqlx::Error::Configuration(
                "the system random generator would not produce a session credential".into(),
            ))
        })?;
    let credential =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
    let digest = SecretDigest::of(&credential);
    Ok((credential, digest))
}

/// Open a session.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails, including when the device binding does not
/// satisfy the schema's rule for the kind.
pub async fn create_session<'e, E>(
    executor: E,
    new: &NewSession<'_>,
) -> Result<Session, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let session_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.sessions
             (session_id, user_id, kind, device_id, audience, issued_at, expires_at, last_seen_at,
              token_hash)
         values ($1, $2, $3, $4, $5, $6, $7, $6, $8)",
    )
    .bind(session_id)
    .bind(new.user_id)
    .bind(new.kind.as_str())
    .bind(new.device_id)
    .bind(new.audience)
    .bind(to_offset(new.issued_at))
    .bind(to_offset(new.expires_at))
    .bind(new.token.map(|token| token.as_bytes().to_vec()))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(Session {
        session_id,
        user_id: new.user_id,
        kind: new.kind,
        device_id: new.device_id,
        audience: new.audience.to_owned(),
        issued_at: new.issued_at,
        expires_at: new.expires_at,
        revoked_at: None,
        last_seen_at: Some(new.issued_at),
    })
}

/// Authenticate a presented bearer credential.
///
/// Returns the session only when the credential matches AND the session is live at `now` AND the
/// audience is the one this listener serves. All three in one query, because a caller that fetched
/// the session and then checked liveness itself would eventually forget one of them.
///
/// A wrong credential, a revoked session, an expired session and a wrong audience are deliberately
/// indistinguishable to the caller: `ARCHITECTURE.md` S15 requires authorization before existence is
/// disclosed, and four different answers here would be an oracle.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn authenticate<'e, E>(
    executor: E,
    presented: SecretDigest,
    audience: &str,
    now: jiff::Timestamp,
) -> Result<Option<Session>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select session_id, user_id, kind, device_id, audience, issued_at, expires_at, revoked_at,
                  last_seen_at
           from identity.sessions
          where token_hash = $1
            and audience = $2
            and revoked_at is null
            and expires_at > $3",
    )
    .bind(presented.as_bytes().as_slice())
    .bind(audience)
    .bind(to_offset(now))
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| {
        let kind: String = row.try_get("kind").map_err(PersistenceError::Query)?;
        let revoked_at: Option<time::OffsetDateTime> =
            row.try_get("revoked_at").map_err(PersistenceError::Query)?;
        Ok(Session {
            session_id: row.try_get("session_id").map_err(PersistenceError::Query)?,
            user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
            kind: SessionKind::from_str_opt(&kind).unwrap_or(SessionKind::Service),
            device_id: row.try_get("device_id").map_err(PersistenceError::Query)?,
            audience: row.try_get("audience").map_err(PersistenceError::Query)?,
            issued_at: from_offset(row.try_get("issued_at").map_err(PersistenceError::Query)?),
            expires_at: from_offset(row.try_get("expires_at").map_err(PersistenceError::Query)?),
            revoked_at: revoked_at.map(from_offset),
            last_seen_at: row
                .try_get::<Option<time::OffsetDateTime>, _>("last_seen_at")
                .map_err(PersistenceError::Query)?
                .map(from_offset),
        })
    })
    .transpose()
}

/// Read a session by identity.
///
/// Returns the row whatever its state: the caller decides what to do with a revoked or expired
/// session, and hiding it here would make a revoked session indistinguishable from a forged id.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn find_session<'e, E>(
    executor: E,
    session_id: Uuid,
) -> Result<Option<Session>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select session_id, user_id, kind, device_id, audience, issued_at, expires_at, revoked_at,
                  last_seen_at
           from identity.sessions where session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| {
        let kind: String = row.try_get("kind").map_err(PersistenceError::Query)?;
        let revoked_at: Option<time::OffsetDateTime> =
            row.try_get("revoked_at").map_err(PersistenceError::Query)?;
        Ok(Session {
            session_id: row.try_get("session_id").map_err(PersistenceError::Query)?,
            user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
            // An unknown token cannot pass the CHECK constraint. `Service` is the safe fallback:
            // it is the kind with no device binding and no client-facing rotation.
            kind: SessionKind::from_str_opt(&kind).unwrap_or(SessionKind::Service),
            device_id: row.try_get("device_id").map_err(PersistenceError::Query)?,
            audience: row.try_get("audience").map_err(PersistenceError::Query)?,
            issued_at: from_offset(row.try_get("issued_at").map_err(PersistenceError::Query)?),
            expires_at: from_offset(row.try_get("expires_at").map_err(PersistenceError::Query)?),
            revoked_at: revoked_at.map(from_offset),
            last_seen_at: row
                .try_get::<Option<time::OffsetDateTime>, _>("last_seen_at")
                .map_err(PersistenceError::Query)?
                .map(from_offset),
        })
    })
    .transpose()
}

/// Revoke one session.
///
/// Idempotent: revoking an already-revoked session keeps the first instant, because the first one
/// is the one the audit trail and any incident timeline care about.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn revoke_session<'e, E>(
    executor: E,
    session_id: Uuid,
    revoked_at: jiff::Timestamp,
) -> Result<bool, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let result = sqlx::query(
        "update identity.sessions set revoked_at = $2
          where session_id = $1 and revoked_at is null",
    )
    .bind(session_id)
    .bind(to_offset(revoked_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(result.rows_affected() == 1)
}

/// Revoke every live session of a user.
///
/// The blast radius of a suspected compromise. Returns how many were revoked.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn revoke_all_sessions_of_user<'e, E>(
    executor: E,
    user_id: Uuid,
    revoked_at: jiff::Timestamp,
) -> Result<u64, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let result = sqlx::query(
        "update identity.sessions set revoked_at = $2
          where user_id = $1 and revoked_at is null",
    )
    .bind(user_id)
    .bind(to_offset(revoked_at))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(result.rows_affected())
}

/// A session as the lifecycle listing presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    /// The session's identity.
    pub session_id: Uuid,
    /// Its owner.
    pub user_id: Uuid,
    /// How it was established.
    pub kind: SessionKind,
    /// The bound device, when the kind requires one.
    pub device: Option<crate::device::DeviceRef>,
    /// When it was issued.
    pub issued_at: jiff::Timestamp,
    /// When it stops being valid on its own.
    pub expires_at: jiff::Timestamp,
    /// When it was last used, as far as the throttled touch records.
    pub last_seen_at: Option<jiff::Timestamp>,
}

/// List one user's live sessions, newest first, cursor-paginated.
///
/// Live means unrevoked and unexpired at `now`. The cursor names the LAST row of the previous
/// page; rows strictly older than it follow, so writes during a walk can neither duplicate nor
/// skip a row behind the cursor.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn list_live_sessions<'e, E>(
    executor: E,
    user_id: Uuid,
    now: jiff::Timestamp,
    after: Option<(jiff::Timestamp, Uuid)>,
    limit: i64,
) -> Result<Vec<ListedSession>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    // One row beyond the limit answers "is there a next page" without a second query. The two
    // statement shapes differ only in their cursor predicate, so one execution site serves both.
    let fetch = limit + 1;
    let sql_with_cursor =
        "select s.session_id, s.user_id, s.kind, s.issued_at, s.expires_at, s.last_seen_at,
                    d.device_id as device_id, d.kind as device_kind, d.display_name as device_name
               from identity.sessions s
               left join identity.registered_devices d on d.device_id = s.device_id
              where s.user_id = $1
                and s.revoked_at is null
                and s.expires_at > $2
                and (s.issued_at, s.session_id) < ($3, $4)
              order by s.issued_at desc, s.session_id desc
              limit $5";
    let sql_first_page =
        "select s.session_id, s.user_id, s.kind, s.issued_at, s.expires_at, s.last_seen_at,
                    d.device_id as device_id, d.kind as device_kind, d.display_name as device_name
               from identity.sessions s
               left join identity.registered_devices d on d.device_id = s.device_id
              where s.user_id = $1
                and s.revoked_at is null
                and s.expires_at > $2
              order by s.issued_at desc, s.session_id desc
              limit $3";
    let mut query = match after {
        Some((_, _)) => sqlx::query(sql_with_cursor)
            .bind(user_id)
            .bind(to_offset(now)),
        None => sqlx::query(sql_first_page)
            .bind(user_id)
            .bind(to_offset(now)),
    };
    if let Some((issued_at, session_id)) = after {
        query = query.bind(to_offset(issued_at)).bind(session_id);
    }
    let _ = fetch;
    let rows = query
        .bind(fetch)
        .fetch_all(executor)
        .await
        .map_err(PersistenceError::Query)?;

    let mut sessions: Vec<ListedSession> = Vec::new();
    for row in rows {
        let kind: String = row.try_get("kind").map_err(PersistenceError::Query)?;
        let device_id: Option<Uuid> = row.try_get("device_id").map_err(PersistenceError::Query)?;
        let device_kind: Option<String> = row
            .try_get("device_kind")
            .map_err(PersistenceError::Query)?;
        let device: Option<crate::device::DeviceRef> = match (device_id, device_kind) {
            (Some(id), Some(kind)) => Some(crate::device::DeviceRef {
                display_name: row
                    .try_get("device_name")
                    .map_err(PersistenceError::Query)?,
                kind: DeviceKind::from_str_opt(&kind).unwrap_or(DeviceKind::ExportAgent),
                device_id: id,
            }),
            // A live `device` session always names its device; anything else has none.
            _ => None,
        };
        sessions.push(ListedSession {
            session_id: row.try_get("session_id").map_err(PersistenceError::Query)?,
            user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
            kind: SessionKind::from_str_opt(&kind).unwrap_or(SessionKind::Service),
            issued_at: from_offset(row.try_get("issued_at").map_err(PersistenceError::Query)?),
            expires_at: from_offset(row.try_get("expires_at").map_err(PersistenceError::Query)?),
            last_seen_at: row
                .try_get::<Option<time::OffsetDateTime>, _>("last_seen_at")
                .map_err(PersistenceError::Query)?
                .map(from_offset),
            device,
        });
    }
    sessions.truncate(usize::try_from(limit).unwrap_or(0));
    Ok(sessions)
}

/// Touch a session's — and optionally its device's — last-seen instant.
///
/// Throttled: at most one write per `min_interval`, evaluated against the stored instant inside
/// the UPDATE itself, so concurrent requests cannot make the write hotter than the interval.
/// Best-effort by contract; the caller decides what a failure means (it must not fail a request
/// authentication already admitted).
///
/// Takes the pool directly: it always runs standalone, never inside another operation's
/// transaction, and its two updates share one executor.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails.
pub async fn touch_last_seen(
    executor: &sqlx::PgPool,
    session_id: Uuid,
    device_id: Option<Uuid>,
    now: jiff::Timestamp,
    min_interval: jiff::SignedDuration,
) -> Result<(), PersistenceError> {
    let instant = to_offset(now);
    let stale_before = to_offset(now - min_interval);

    // The throttle lives in the WHERE clause: concurrent requests cannot make the write hotter
    // than the interval, because only one of them sees a stale-enough instant.
    sqlx::query(
        "update identity.sessions set last_seen_at = $2
          where session_id = $1
            and revoked_at is null
            and (last_seen_at is null or last_seen_at <= $3)",
    )
    .bind(session_id)
    .bind(instant)
    .bind(stale_before)
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    if let Some(device_id) = device_id {
        sqlx::query(
            "update identity.registered_devices set last_seen_at = $2
              where device_id = $1
                and revoked_at is null
                and (last_seen_at is null or last_seen_at <= $3)",
        )
        .bind(device_id)
        .bind(instant)
        .bind(stale_before)
        .execute(executor)
        .await
        .map_err(PersistenceError::Query)?;
    }

    Ok(())
}

/// The identities of one user's live sessions, unsorted and unpaginated on purpose.
///
/// The revoke-all path needs exactly this: which rows its single UPDATE is about to close, so
/// each can receive a durable why. Bounded by how many sessions one user realistically holds.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn list_live_session_ids<'e, E>(
    executor: E,
    user_id: Uuid,
    now: jiff::Timestamp,
) -> Result<Vec<Uuid>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    sqlx::query_scalar(
        "select session_id from identity.sessions
          where user_id = $1 and revoked_at is null and expires_at > $2",
    )
    .bind(user_id)
    .bind(to_offset(now))
    .fetch_all(executor)
    .await
    .map_err(PersistenceError::Query)
}
