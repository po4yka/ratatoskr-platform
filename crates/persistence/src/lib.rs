//! The `PostgreSQL` pool Platform owns, the migrator embedded in the binary, and the readiness probe
//! for both.
//!
//! Scope. This crate owns the `identity` and `operations` schemas and nothing else. `ARCHITECTURE.md`
//! S4.2 and S19 invariant 6 forbid this pool ever reaching a domain service's tables, and the only
//! thing that keeps that true over time is that no code outside `ratatoskr-platform-identity` and
//! `ratatoskr-platform-operations` is given a reason to hold a [`Database`].
//!
//! Migrations live in one directory, `migrations/`, not the two that `ARCHITECTURE.md` S3 draws.
//! `sqlx::Migrator` records applied versions in a single `_sqlx_migrations` table and exposes no way
//! to change that table's name (verified against sqlx 0.8.6: the only setters are
//! `set_ignore_missing` and `set_locking`), so two directories would share one ledger and collide on
//! version numbers. The owning schema is carried in each file name instead. See
//! `docs/adr/0004-migration-layout.md`.

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
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// The migrations, embedded at compile time.
///
/// Embedded rather than read from disk so a deployed binary cannot be paired with a different
/// schema than the one it was built against. The path is relative to this crate's manifest.
static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// A failure in the pool, a migration, or a query.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PersistenceError {
    /// The pool could not be created, or a connection could not be acquired.
    #[error("the database connection could not be established")]
    Connect(#[source] sqlx::Error),

    /// A migration failed to apply, or the applied set does not match the embedded set.
    #[error("the database schema could not be brought up to date")]
    Migrate(#[source] sqlx::migrate::MigrateError),

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

    /// Apply every embedded migration that has not been applied.
    ///
    /// Idempotent, and safe to run from more than one instance at once: `sqlx` takes a `PostgreSQL`
    /// advisory lock for the duration, so a rolling deployment of three replicas applies each
    /// migration once.
    ///
    /// # Errors
    ///
    /// [`PersistenceError::Migrate`] if a migration fails, or if a migration that this binary does
    /// not contain has already been applied — which means the database is newer than the binary and
    /// continuing would corrupt it.
    pub async fn migrate(&self) -> Result<(), PersistenceError> {
        MIGRATOR
            .run(&self.pool)
            .await
            .map_err(PersistenceError::Migrate)
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

/// The migrations this binary carries, for the version endpoint and for tests.
///
/// A deployment that cannot say which schema it expects cannot be diagnosed.
#[must_use]
pub fn embedded_migration_versions() -> Vec<i64> {
    MIGRATOR.iter().map(|migration| migration.version).collect()
}
