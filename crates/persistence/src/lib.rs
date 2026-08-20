//! The `PostgreSQL` pool Platform owns, the schema embedded in the binary, and the readiness probe
//! for both.
//!
//! Scope. This crate owns the `identity` and `operations` schemas and nothing else. `ARCHITECTURE.md`
//! S4.2 and S19 invariant 6 forbid this pool ever reaching a domain service's tables, and the only
//! thing that keeps that true over time is that no code outside `ratatoskr-platform-identity` and
//! `ratatoskr-platform-operations` is given a reason to hold a [`Database`].
//!
//! The schema is ONE file, `schema.sql` at the repository root, not the two directories
//! `ARCHITECTURE.md` S3 once drew and not a numbered ledger. No database holds data that has to survive
//! a schema change, so an incremental history buys nothing and costs a rule that an applied file
//! can never be edited. A schema change edits `schema.sql` in place. See
//! `docs/adr/0004-migration-layout-and-query-checking.md`.

#[cfg(feature = "test-support")]
pub mod test_support;

use std::time::Duration;

/// How long a pooled connection may sit idle before it is closed rather than handed out.
///
/// Ten minutes. A pooled connection that outlives a database restart is the classic "it works on
/// the second try" failure, and this is the knob that bounds how long that window can be.
const IDLE_TIMEOUT: Duration = Duration::from_mins(10);

use platform_core::{PlatformError, Subsystem};
use secrecy::ExposeSecret as _;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// The schema, embedded at compile time.
///
/// Embedded rather than read from disk so a deployed binary cannot be paired with a different
/// schema than the one it was built against. `include_str!` makes the file a build input, so
/// editing it rebuilds this crate and every artifact that links it — which is the whole of the
/// staleness protection a build script and a directory-listing test used to provide for a
/// directory of files. The path is relative to this source file.
const SCHEMA: &str = include_str!("../../../schema.sql");

/// The advisory-lock key `apply_schema` holds while it decides and applies.
///
/// One arbitrary but fixed 64-bit value; `PostgreSQL` advisory locks are a namespace of integers
/// with no meaning of their own, and nothing else in this system takes one. Kept because ADR-0010
/// founds it on a case that still happens with exactly one process per role: a restart that
/// overlaps the previous process's grace window is two processes, for a few seconds, and both call
/// this method.
const SCHEMA_LOCK: i64 = 0x7261_7461_736b_7201;

/// A failure in the pool, the schema, or a query.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PersistenceError {
    /// The pool could not be created, or a connection could not be acquired.
    #[error("the database connection could not be established")]
    Connect(#[source] sqlx::Error),

    /// The schema could not be applied.
    #[error("the database schema could not be applied")]
    Schema(#[source] sqlx::Error),

    /// A query failed.
    #[error("a database query failed")]
    Query(#[source] sqlx::Error),
}

impl From<PersistenceError> for PlatformError {
    /// Every persistence failure is internal. There is no variant a client learns about: a
    /// connection string, a constraint name and a query are all internal detail, and
    /// `ARCHITECTURE.md` S15 requires the public surface to carry none of them.
    fn from(error: PersistenceError) -> Self {
        Self::Internal {
            subsystem: Subsystem::Persistence,
            source: Box::new(error),
        }
    }
}

/// The pool, and the only handle through which Platform reaches `PostgreSQL`.
#[derive(Debug, Clone)]
pub struct Database {
    pub(crate) pool: PgPool,
}

impl Database {
    /// Create the pool and verify it can serve one connection.
    ///
    /// The verification is not ceremony: `PgPoolOptions::connect` is lazy about nothing but it is
    /// still possible to hold a pool whose credentials are wrong, and finding that out on the first
    /// request rather than at startup is how a deployment reports itself healthy and then fails
    /// every call.
    ///
    /// # Errors
    ///
    /// [`PersistenceError::Connect`] if the URL is unusable or the server refuses the connection
    /// within the configured acquire timeout.
    pub async fn connect(config: &platform_core::DatabaseConfig) -> Result<Self, PersistenceError> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(Duration::from_secs(config.acquire_timeout_seconds))
            .idle_timeout(IDLE_TIMEOUT)
            .test_before_acquire(true)
            .connect(config.url.expose_secret())
            .await
            .map_err(PersistenceError::Connect)?;

        Ok(Self { pool })
    }

    /// Apply [`SCHEMA`] to a database that does not have it yet.
    ///
    /// Idempotent, and safe to run while another process is still holding connections. One
    /// transaction does all three things: it takes a `PostgreSQL` advisory lock, asks whether
    /// `identity` exists, and applies the file only if it does not. The lock is transaction-scoped,
    /// so it is released by the commit and by a panic alike, and a second process that arrives
    /// during a restart waits for the first, then sees the schema and does nothing. That is why the
    /// lock still matters with exactly one process per role (ADR-0010) — a restart IS two
    /// processes, for a few seconds.
    ///
    /// `PostgreSQL` DDL is transactional, so a file that fails halfway leaves the database exactly
    /// as it was rather than half-applied. The presence check is therefore an honest question:
    /// either every object in the file is there or none of it is.
    ///
    /// # Errors
    ///
    /// [`PersistenceError::Schema`] if the lock cannot be taken, the catalogue cannot be read, or a
    /// statement in the file fails.
    pub async fn apply_schema(&self) -> Result<(), PersistenceError> {
        let mut transaction = self.pool.begin().await.map_err(PersistenceError::Schema)?;
        lock_and_apply(&mut transaction)
            .await
            .map_err(PersistenceError::Schema)?;
        transaction.commit().await.map_err(PersistenceError::Schema)
    }

    /// Answer whether the database is usable right now.
    ///
    /// Deliberately a round trip and not a pool-state inspection: a pool with idle connections to a
    /// server that is refusing queries looks healthy from the inside.
    ///
    /// # Errors
    ///
    /// [`PersistenceError::Query`] if the round trip fails or times out.
    pub async fn ping(&self) -> Result<(), PersistenceError> {
        sqlx::query("select 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(PersistenceError::Query)
    }

    /// The pool, for the two crates that own a schema.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Close the pool and wait for checked-out connections to be returned.
    ///
    /// Called from the shutdown sequence after the listener stops accepting, so an in-flight
    /// request keeps its connection through the grace window.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// The body of [`Database::apply_schema`], on one connection so the lock and the apply share a
/// session.
///
/// A free function taking `&mut PgConnection` by its named type: `PublicRoutes::build` is an async
/// trait method, so `ratatoskr-edge`'s caller has to prove this future is `Send`, and that proof
/// needs the executor's lifetime pinned rather than inferred at the call site
/// (rust-lang/rust#100013, seen as "implementation of `Executor` is not general enough").
///
/// The file goes through `Executor::execute` and NOT `sqlx::raw_sql`, which trips the same bound.
/// Both send the string over the simple query protocol, which runs every statement in it; `execute`
/// folds the per-statement results into one.
async fn lock_and_apply(connection: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(SCHEMA_LOCK)
        .execute(&mut *connection)
        .await?;

    // The first schema the file creates. Under the lock, its absence means the file has never been
    // applied to this database.
    let present: Option<String> = sqlx::query_scalar("select to_regnamespace('identity')::text")
        .fetch_one(&mut *connection)
        .await?;

    if present.is_none() {
        sqlx::Executor::execute(connection, SCHEMA).await?;
    }

    Ok(())
}
