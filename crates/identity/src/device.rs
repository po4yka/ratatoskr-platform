//! Registered devices: a mobile app, a browser extension or the export agent.
//!
//! `DOMAIN.md` calls a device "a registered installation with constrained credentials". The
//! constrained part is why a device has its own row rather than being a session attribute: a device
//! outlives the sessions it opens, and revoking it must invalidate all of them at once.

use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::{SecretDigest, from_offset, to_offset};

/// Which client installed itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// The mobile client.
    Mobile,
    /// The browser extension.
    BrowserExtension,
    /// The macOS export agent.
    ExportAgent,
}

impl DeviceKind {
    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mobile => "mobile",
            Self::BrowserExtension => "browser_extension",
            Self::ExportAgent => "export_agent",
        }
    }

    /// Parse a stored token.
    #[must_use]
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "mobile" => Some(Self::Mobile),
            "browser_extension" => Some(Self::BrowserExtension),
            "export_agent" => Some(Self::ExportAgent),
            _ => None,
        }
    }
}

/// A registered installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The device's identity.
    pub device_id: Uuid,
    /// Its owner.
    pub user_id: Uuid,
    /// Which client.
    pub kind: DeviceKind,
    /// A human label the user chose. Never used for authorization.
    pub display_name: Option<String>,
    /// When it was registered.
    pub created_at: jiff::Timestamp,
    /// When it was revoked, if it was.
    pub revoked_at: Option<jiff::Timestamp>,
}

impl Device {
    /// Whether this device may open a session.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// Register a device.
///
/// The secret is presented as a digest and stored as one; this function cannot receive, and the
/// schema cannot hold, the credential itself.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails.
pub async fn register_device<'e, E>(
    executor: E,
    user_id: Uuid,
    kind: DeviceKind,
    display_name: Option<&str>,
    digest: SecretDigest,
    now: jiff::Timestamp,
) -> Result<Device, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let device_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.registered_devices
             (device_id, user_id, kind, display_name, secret_hash, created_at, last_seen_at)
         values ($1, $2, $3, $4, $5, $6, $6)",
    )
    .bind(device_id)
    .bind(user_id)
    .bind(kind.as_str())
    .bind(display_name)
    .bind(digest.as_bytes().as_slice())
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(Device {
        device_id,
        user_id,
        kind,
        display_name: display_name.map(str::to_owned),
        created_at: now,
        revoked_at: None,
    })
}

/// Read a device by identity.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn find_device<'e, E>(
    executor: E,
    device_id: Uuid,
) -> Result<Option<Device>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select device_id, user_id, kind, display_name, created_at, revoked_at
           from identity.registered_devices where device_id = $1",
    )
    .bind(device_id)
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    row.map(|row| {
        let kind: String = row.try_get("kind").map_err(PersistenceError::Query)?;
        let revoked_at: Option<time::OffsetDateTime> =
            row.try_get("revoked_at").map_err(PersistenceError::Query)?;
        Ok(Device {
            device_id: row.try_get("device_id").map_err(PersistenceError::Query)?,
            user_id: row.try_get("user_id").map_err(PersistenceError::Query)?,
            // Unreachable through the CHECK constraint. `ExportAgent` is the fallback because it is
            // the kind with the narrowest capability set, so an unknown device is the least
            // privileged one rather than the most.
            kind: DeviceKind::from_str_opt(&kind).unwrap_or(DeviceKind::ExportAgent),
            display_name: row
                .try_get("display_name")
                .map_err(PersistenceError::Query)?,
            created_at: from_offset(row.try_get("created_at").map_err(PersistenceError::Query)?),
            revoked_at: revoked_at.map(from_offset),
        })
    })
    .transpose()
}

/// Verify a presented device secret against the stored digest, in the database.
///
/// The comparison happens in SQL so the stored digest never enters the process, and so a caller
/// cannot accidentally compare it with `==` on a `Vec<u8>`, which is not constant time.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn verify_device_secret<'e, E>(
    executor: E,
    device_id: Uuid,
    presented: SecretDigest,
) -> Result<bool, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select (secret_hash = $2) as matches
           from identity.registered_devices
          where device_id = $1 and revoked_at is null",
    )
    .bind(device_id)
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    match row {
        Some(row) => row
            .try_get::<bool, _>("matches")
            .map_err(PersistenceError::Query),
        // A revoked or unknown device is a failed verification, not an error: telling the two apart
        // would disclose whether a device id exists (ARCHITECTURE S15, authorize before disclosing
        // existence).
        None => Ok(false),
    }
}

/// Revoke a device and every session bound to it, in one transaction.
///
/// Revoking the device alone would leave its live sessions authenticating, which is the privilege
/// escalation `THREAT_MODEL.md` names.
///
/// # Errors
///
/// [`PersistenceError::Query`] if a statement fails.
pub async fn revoke_device(
    transaction: &mut sqlx::PgTransaction<'_>,
    device_id: Uuid,
    revoked_at: jiff::Timestamp,
) -> Result<Vec<Uuid>, PersistenceError> {
    sqlx::query(
        "update identity.registered_devices set revoked_at = $2
          where device_id = $1 and revoked_at is null",
    )
    .bind(device_id)
    .bind(to_offset(revoked_at))
    .execute(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    let sessions: Vec<Uuid> = sqlx::query_scalar(
        "update identity.sessions set revoked_at = $2
          where device_id = $1 and revoked_at is null
         returning session_id",
    )
    .bind(device_id)
    .bind(to_offset(revoked_at))
    .fetch_all(&mut **transaction)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(sessions)
}

/// A device as the listing presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRef {
    /// The device's identity.
    pub device_id: Uuid,
    /// Which client.
    pub kind: DeviceKind,
    /// The human label, when the owner gave one.
    pub display_name: Option<String>,
}

/// One active device in the owner's listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedDevice {
    /// The device's identity.
    pub device_id: Uuid,
    /// Which client.
    pub kind: DeviceKind,
    /// The human label, when the owner gave one.
    pub display_name: Option<String>,
    /// When it was registered.
    pub created_at: jiff::Timestamp,
    /// When it was last used, as far as the throttled touch records.
    pub last_seen_at: Option<jiff::Timestamp>,
}

/// Verify a presented root secret against an active device and answer its owner.
///
/// One statement, so there is no window between "is this device acceptable" and "whose is it".
/// The row lock it takes serializes against revocation: a deletion committing concurrently makes
/// this verification fail rather than granting a session onto a dying device. Use inside the
/// caller's transaction.
///
/// A wrong secret, an unknown identifier and a revoked device are the same `None` — the caller is
/// untrusted, and three different answers would be an oracle (`ARCHITECTURE.md` S15).
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn authenticate_device<'e, E>(
    executor: E,
    device_id: Uuid,
    presented: SecretDigest,
) -> Result<Option<Uuid>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let row: Option<(Uuid,)> = sqlx::query_as(
        "select user_id
           from identity.registered_devices
          where device_id = $1
            and revoked_at is null
            and secret_hash = $2
          for update",
    )
    .bind(device_id)
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    Ok(row.map(|(user_id,)| user_id))
}

/// List one user's active devices, newest first, cursor-paginated.
///
/// Revoked devices are history, not manageable state, and never appear here.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn list_active_devices<'e, E>(
    executor: E,
    user_id: Uuid,
    after: Option<(jiff::Timestamp, Uuid)>,
    limit: i64,
) -> Result<Vec<ListedDevice>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let rows = if let Some((created_at, device_id)) = after {
        sqlx::query(
            "select device_id, kind, display_name, created_at, last_seen_at
               from identity.registered_devices
              where user_id = $1
                and revoked_at is null
                and (created_at, device_id) < ($2, $3)
              order by created_at desc, device_id desc
              limit $4",
        )
        .bind(user_id)
        .bind(to_offset(created_at))
        .bind(device_id)
        .bind(limit + 1)
        .fetch_all(executor)
        .await
    } else {
        sqlx::query(
            "select device_id, kind, display_name, created_at, last_seen_at
               from identity.registered_devices
              where user_id = $1
                and revoked_at is null
              order by created_at desc, device_id desc
              limit $2",
        )
        .bind(user_id)
        .bind(limit + 1)
        .fetch_all(executor)
        .await
    }
    .map_err(PersistenceError::Query)?;

    let mut devices = Vec::new();
    for row in rows {
        let kind: String = row.try_get("kind").map_err(PersistenceError::Query)?;
        devices.push(ListedDevice {
            device_id: row.try_get("device_id").map_err(PersistenceError::Query)?,
            kind: DeviceKind::from_str_opt(&kind).unwrap_or(DeviceKind::ExportAgent),
            display_name: row
                .try_get("display_name")
                .map_err(PersistenceError::Query)?,
            created_at: from_offset(row.try_get("created_at").map_err(PersistenceError::Query)?),
            last_seen_at: row
                .try_get::<Option<time::OffsetDateTime>, _>("last_seen_at")
                .map_err(PersistenceError::Query)?
                .map(from_offset),
        });
    }
    devices.truncate(usize::try_from(limit).unwrap_or(0));
    Ok(devices)
}
