//! `schema.sql` applies, and applying it again does nothing — test M-1.
//!
//! It replaces the test that compared the embedded migration versions with the files in
//! `migrations/`. That hazard was specific to `sqlx::migrate!`, which emitted change tracking per
//! FILE, so a newly added file was tracked by nothing and an already-built artifact kept the set it
//! was compiled with. `include_str!` tracks the one file it reads, so cargo rebuilds this crate
//! whenever `schema.sql` changes and there is no set to drift.
//!
//! What is worth checking instead is the one branch that replaced the ledger. `sqlx` knew a database
//! was up to date by reading `_sqlx_migrations`; [`Database::apply_schema`] knows by asking whether
//! `identity` exists, under an advisory lock. Get that wrong and the second start of a process fails
//! with `schema "identity" already exists` — which is every restart, on the one host there is.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_core::DatabaseConfig;
use platform_persistence::Database;
use sqlx::Executor as _;
use sqlx::postgres::PgPoolOptions;

/// Where the disposable database is created.
///
/// The same variable and the same default the rest of the suite uses, so `docker compose up -d`
/// followed by `cargo test` needs no further setup.
#[expect(
    clippy::disallowed_methods,
    reason = "the workspace bans direct environment reads so that configuration has exactly one \
              loader. This is a test binary choosing where it may create and drop a database."
)]
fn admin_url() -> String {
    std::env::var("PLATFORM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://platform:platform@127.0.0.1:5432/platform".to_owned())
}

fn config(url: &str) -> DatabaseConfig {
    DatabaseConfig {
        url: url.to_owned().into(),
        max_connections: 2,
        acquire_timeout_seconds: 5,
    }
}

/// M-1. A fresh database gets all three schemas, and a second apply is a no-op.
#[tokio::test]
async fn the_schema_applies_once_and_tolerates_being_applied_again() {
    let admin = admin_url();
    let name = format!("platform_schema_{}", uuid::Uuid::now_v7().simple());

    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin)
        .await
        .expect("the test database server must be reachable");
    // The same three clauses `test_support` and `deploy/postgres/01-database-and-roles.sql` use: the
    // collation is stated rather than inherited, so a text index behaves the same here as where it
    // is built. The name comes from a UUID and can carry no injection.
    admin_pool
        .execute(
            format!(
                r#"create database "{name}" template template0
                   locale_provider icu icu_locale 'und-x-icu' encoding 'UTF8'"#
            )
            .as_str(),
        )
        .await
        .expect("a disposable database");

    let (prefix, _) = admin.rsplit_once('/').unwrap_or((admin.as_str(), ""));
    let database = Database::connect(&config(&format!("{prefix}/{name}")))
        .await
        .expect("the disposable database must be reachable");

    database.apply_schema().await.expect("the first apply");
    database
        .apply_schema()
        .await
        .expect("the second apply must be a no-op, because every restart is one");

    // One table from each of the three schemas: a file that stopped halfway would leave the first
    // schema present, which is exactly what the presence check looks at.
    for table in [
        "identity.users",
        "operations.operations",
        "platform_ingest.webhook_sources",
    ] {
        let present: Option<String> =
            sqlx::query_scalar(&format!("select to_regclass('{table}')::text"))
                .fetch_one(database.pool())
                .await
                .expect("the catalogue must be readable");
        assert!(present.is_some(), "{table} must exist after apply_schema");
    }

    database.close().await;
    admin_pool
        .execute(format!(r#"drop database "{name}" with (force)"#).as_str())
        .await
        .expect("the disposable database must be droppable");
    admin_pool.close().await;
}
