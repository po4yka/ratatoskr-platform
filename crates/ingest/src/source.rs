//! Who is pushing, and whether we know them.
//!
//! `ARCHITECTURE.md` S9 step 1: "source authentication or signature validation". Authentication is
//! what this implements, and ADR-0009 records why: an HMAC signature needs the shared secret back
//! in plaintext, so a store that can validate one must be able to return one, and that needs key
//! management this repository does not have. A bearer credential stored as a SHA-256 digest needs
//! none, and is hashed exactly the way `identity.sessions` hashes a session credential.

use platform_identity::SecretDigest;
use platform_persistence::PersistenceError;
use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::Target;

/// A registered source that pushes signals to Platform.
#[derive(Debug, Clone)]
pub struct WebhookSource {
    /// The source, as it appears in its own URL.
    pub source_id: Uuid,
    /// The user whose operation a signal from this source creates.
    pub owner_user_id: Uuid,
    /// Where its signals are routed.
    pub target: Target,
    /// The operator-facing name, for a log record. Never returned to the caller: a source learns
    /// nothing from us about how it is filed.
    pub label: String,
}

/// A source that could not be resolved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceError {
    /// The database refused or failed. Not an authentication failure, and must never be reported
    /// as one: telling a caller "unknown credential" when the database is down sends them to
    /// rotate a credential that was never the problem.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// The row names a target this build does not serve. An operator wrote it; the source did
    /// nothing wrong and can do nothing about it, so it is reported as ours rather than theirs.
    #[error("source {source_id} routes to `{target}`, which this build does not serve")]
    UnknownTarget {
        /// The misconfigured source.
        source_id: Uuid,
        /// What its row asked for.
        target: String,
    },
}

/// The source that presents this credential, if any.
///
/// A disabled source resolves to `None`, indistinguishably from an unknown one: `disabled_at` is
/// set precisely when we have decided to stop listening to somebody, and telling them which of the
/// two states they are in is a courtesy owed to nobody.
///
/// # Errors
///
/// [`SourceError::Persistence`] if the query fails, [`SourceError::UnknownTarget`] if the row names
/// a target this build cannot route to.
pub async fn authenticate<'e, E>(
    executor: E,
    presented: SecretDigest,
) -> Result<Option<WebhookSource>, SourceError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query(
        "select source_id, owner_user_id, target, label
           from platform_ingest.webhook_sources
          where token_hash = $1 and disabled_at is null",
    )
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let source_id: Uuid = row.try_get("source_id").map_err(PersistenceError::Query)?;
    let stored: String = row.try_get("target").map_err(PersistenceError::Query)?;
    let Some(target) = Target::parse(&stored) else {
        return Err(SourceError::UnknownTarget {
            source_id,
            target: stored,
        });
    };

    Ok(Some(WebhookSource {
        source_id,
        owner_user_id: row
            .try_get("owner_user_id")
            .map_err(PersistenceError::Query)?,
        target,
        label: row.try_get("label").map_err(PersistenceError::Query)?,
    }))
}

/// Register a source, returning its identifier.
///
/// There is no HTTP route for this at milestone 7 and deliberately so: registering a source is an
/// operator action with no client-facing half yet, and a route for it would need its own
/// authorization model — who may create a source, and on whose behalf — which is the OAuth and
/// grant work of milestone 8. Until then a source is created by an operator through this function,
/// and the function exists so that creation still goes through the same bounds and the same closed
/// target list the request path relies on.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails — including when the label exceeds the column's
/// bound or the credential is already registered to another source.
pub async fn register<'e, E>(
    executor: E,
    owner_user_id: Uuid,
    label: &str,
    token: SecretDigest,
    target: Target,
    now: jiff::Timestamp,
) -> Result<Uuid, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let source_id = Uuid::now_v7();
    sqlx::query(
        "insert into platform_ingest.webhook_sources
             (source_id, owner_user_id, label, token_hash, target, created_at)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(source_id)
    .bind(owner_user_id)
    .bind(label)
    .bind(token.as_bytes().as_slice())
    .bind(target.as_str())
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(source_id)
}

/// `jiff` on the wire, `time` in the driver. The two convert through unix nanoseconds, which needs
/// no calendar and cannot be ambiguous.
fn to_offset(instant: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(instant.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
