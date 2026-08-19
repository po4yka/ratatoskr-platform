//! The `ratatoskr-ingest` deployable.
//!
//! Milestone 7, and the first milestone at which this binary does anything. It binds a public
//! listener and serves the generic webhook adapter of `ARCHITECTURE.md` S9: a registered source
//! pushes a signal, and the signal becomes a durable operation and a command in the outbox.
//!
//! # Two things it deliberately does not do
//!
//! **It does not migrate.** S18 gives ingest its own least-privilege database role, and a role that
//! may create a schema is not one. `ratatoskr-edge` owns the migrations; this process checks that
//! they have been applied and refuses to start if they have not, so the failure is one sentence at
//! startup rather than a Postgres error on the first inbound signal.
//!
//! **It does not publish.** It writes commands into the transactional outbox and stops there; the
//! publisher runs in `ratatoskr-edge`, and ADR-0013 decides that it is the ONLY one. That is a
//! real coupling — with edge down, commands accumulate and nothing sends them — and it is a
//! backlog rather than a loss, because the outbox is the durable half. The consequence for this
//! binary is that it reads no `RATATOSKR__BUS__*` variable and holds no NATS credential at all.

use std::process::ExitCode;
use std::sync::Arc;

use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_http::{RuntimeState, Serving};
use platform_ingest::IngestState;
use platform_persistence::Database;

const ROLE: RuntimeRole = RuntimeRole::Ingest;

/// Ingest's contribution to its public listener.
struct IngestRoutes;

impl platform_http::PublicRoutes for IngestRoutes {
    /// Connect, verify the schema is there, and serve.
    async fn build(
        self,
        config: &PlatformConfig,
        _health: &Arc<RuntimeState>,
    ) -> Result<Serving, String> {
        let Some(database) = config.database.as_ref() else {
            return Err(
                "ratatoskr-ingest resolves every signal against a database and requires \
                 RATATOSKR__DATABASE__URL"
                    .to_owned(),
            );
        };

        let database = Database::connect(database)
            .await
            .map_err(|error| format!("the database could not be reached: {error}"))?;

        let present = platform_ingest::schema_is_present(database.pool())
            .await
            .map_err(|error| format!("the schema could not be inspected: {error}"))?;
        if !present {
            return Err(
                "the platform_ingest schema is absent; ratatoskr-edge applies the \
                        migrations and must have run at least once against this database"
                    .to_owned(),
            );
        }

        Ok(Serving {
            routes: platform_ingest::routes(IngestState::new(database.clone())),
            database: Some(database),
            tasks: Vec::new(),
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("check-config") {
        return platform_http::check_config(ROLE);
    }
    platform_http::run(ROLE, IngestRoutes).await
}
