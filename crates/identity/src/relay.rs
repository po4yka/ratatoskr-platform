//! Carrying an authorization code from a public redirect to the service that owns it.
//!
//! `ARCHITECTURE.md` S6.4 splits the work and ADR-0012 fixes the mechanism: a callback creates one
//! row holding the code, and the owning service claims it exactly once with a service session whose
//! audience matches. The command published to that service carries the relay identifier and never
//! the code, so the outbox, the bus and every log line downstream of them are free of it.
//!
//! Platform validates nothing provider-specific here. It did not generate the `state`, it holds no
//! client secret, and S6.4 gives both to the owning service — so the callback route is an
//! unauthenticated public endpoint accepting attacker-chosen values, and everything it stores is
//! bounded on that basis rather than on the provider being honest.

use sqlx::{PgExecutor, Row as _};
use uuid::Uuid;

use crate::PersistenceError;
use crate::user::IdentityProvider;

/// What a provider redirected back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome<'a> {
    /// The user authorized, and this is the single-use code that proves it.
    Code(&'a str),
    /// The user refused, or the provider failed. Carried through so the owning service can end its
    /// own flow rather than time out waiting for a claim that never comes.
    Error(&'a str),
}

/// A relay the owning service may claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedRelay {
    /// Which provider the callback came from.
    pub provider: IdentityProvider,
    /// The `state` the owning service issued, verbatim and opaque to Platform.
    pub state: String,
    /// The authorization code, when the user authorized.
    pub code: Option<String>,
    /// The provider's error, when they did not.
    pub error: Option<String>,
}

/// Record a callback for the service that owns the provider.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the insert fails, including when a bound is exceeded — every
/// value here is attacker-supplied and the schema is the last check on it.
pub async fn receive<'e, E>(
    executor: E,
    provider: IdentityProvider,
    claim_grant: &str,
    state: &str,
    outcome: CallbackOutcome<'_>,
    now: jiff::Timestamp,
    ttl: jiff::SignedDuration,
) -> Result<Uuid, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let (code, error) = match outcome {
        CallbackOutcome::Code(code) => (Some(code), None),
        CallbackOutcome::Error(error) => (None, Some(error)),
    };
    let relay_id = Uuid::now_v7();
    sqlx::query(
        "insert into identity.oauth_relays
             (relay_id, provider, claim_grant, state, code, error, received_at, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(relay_id)
    .bind(provider.as_str())
    .bind(claim_grant)
    .bind(state)
    .bind(code)
    .bind(error)
    .bind(to_offset(now))
    .bind(to_offset(now + ttl))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(relay_id)
}

/// Claim a relay, exactly once, and destroy the code in the same statement.
///
/// `Ok(None)` covers every reason a claim fails — no such relay, already claimed, expired, or
/// requiring a capability this caller does not hold. They are one answer on purpose: which relays
/// exist, and which capability each needs, is not a caller's business, so the refusals must be
/// indistinguishable (`ARCHITECTURE.md` S15).
///
/// `held` is what the caller was found to hold. It is passed in rather than looked up here so that
/// the grant check and this statement are separately testable, and so the query stays one statement.
///
/// One `update ... returning` rather than a read and a write: two concurrent claims cannot both
/// succeed, because the second matches no row.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn claim<'e, E>(
    executor: E,
    relay_id: Uuid,
    held: &[String],
    now: jiff::Timestamp,
) -> Result<Option<ClaimedRelay>, PersistenceError>
where
    E: PgExecutor<'e>,
{
    // `update ... returning` reports the NEW row, so a plain `returning code` after `set code =
    // null` returns null — the value is destroyed before it can be handed over. A data-modifying
    // CTE reads the row from the statement's snapshot, which is the version before the update, and
    // the join makes the read conditional on the update having happened.
    //
    // The `claimed_at is null` guard is repeated inside the UPDATE and not left to the CTE. Under
    // READ COMMITTED a second concurrent claim blocks on the row lock and then RE-EVALUATES its own
    // WHERE against the new version; without the repeat it would match on `relay_id` alone and claim
    // an already-claimed relay.
    let row = sqlx::query(
        "with target as (
             select relay_id, provider, state, code, error
               from identity.oauth_relays
              where relay_id = $1 and claim_grant = any($2) and claimed_at is null
                and expires_at > $3
         ), claimed as (
             update identity.oauth_relays as r
                set claimed_at = $3, code = null
               from target as t
              where r.relay_id = t.relay_id and r.claimed_at is null
          returning r.relay_id
         )
         select t.provider, t.state, t.code, t.error
           from target as t join claimed as c on c.relay_id = t.relay_id",
    )
    .bind(relay_id)
    .bind(held)
    .bind(to_offset(now))
    .fetch_optional(executor)
    .await
    .map_err(PersistenceError::Query)?;

    let Some(row) = row else {
        return Ok(None);
    };
    let provider: String = row.try_get("provider").map_err(PersistenceError::Query)?;
    Ok(Some(ClaimedRelay {
        // The column's CHECK constraint holds the same closed list, so an unparsable value here
        // would mean the schema and this enum have drifted rather than that a caller sent something.
        provider: IdentityProvider::from_str_opt(&provider).ok_or_else(|| {
            PersistenceError::Query(sqlx::Error::Decode(
                format!(
                    "relay {relay_id} names provider `{provider}`, which this build does not serve"
                )
                .into(),
            ))
        })?,
        state: row.try_get("state").map_err(PersistenceError::Query)?,
        // The pre-update value, read through the CTE above: the code this claim just destroyed. It
        // is therefore returned exactly once, to exactly one caller.
        code: row.try_get("code").map_err(PersistenceError::Query)?,
        error: row.try_get("error").map_err(PersistenceError::Query)?,
    }))
}

/// Remove relays nobody claimed before they expired.
///
/// An unclaimed relay holds a code that is already useless — every provider expires one in minutes —
/// but "already useless" is not a retention policy. Returns how many rows went.
///
/// # Errors
///
/// [`PersistenceError::Query`] if the statement fails.
pub async fn collect_expired<'e, E>(
    executor: E,
    now: jiff::Timestamp,
) -> Result<u64, PersistenceError>
where
    E: PgExecutor<'e>,
{
    let done = sqlx::query(
        "delete from identity.oauth_relays where expires_at <= $1 and claimed_at is null",
    )
    .bind(to_offset(now))
    .execute(executor)
    .await
    .map_err(PersistenceError::Query)?;
    Ok(done.rows_affected())
}

/// `jiff` on the wire, `time` in the driver, through unix nanoseconds.
fn to_offset(instant: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(instant.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
