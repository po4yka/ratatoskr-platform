//! The `ratatoskr-edge` deployable.
//!
//! Milestones 1 to 5: typed configuration, telemetry, the operator listener, and the public API —
//! authenticated capture submission and operation reads.

use std::process::ExitCode;

use axum::Router;
use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_persistence::Database;

const ROLE: RuntimeRole = RuntimeRole::Edge;

/// The audience a session must carry to authenticate here.
///
/// Fixed by the binary, not read from configuration: an operator who could retarget the audience
/// could make a token minted for another surface valid at this one.
const AUDIENCE: &str = "edge";

/// Edge's contribution to its public listener.
struct EdgeRoutes;

impl platform_http::PublicRoutes for EdgeRoutes {
    /// Connect, migrate, and build the routes.
    ///
    /// Refusing to start without a database is deliberate. Every route this binary serves reads or
    /// writes one, so a process that started without it would report itself ready and then fail
    /// every request — which is worse than not starting.
    async fn build(self, config: &PlatformConfig) -> Result<(Router, Option<Database>), String> {
        let Some(database) = config.database.as_ref() else {
            return Err(
                "ratatoskr-edge serves the public API and requires RATATOSKR__DATABASE__URL"
                    .to_owned(),
            );
        };

        let database = Database::connect(database)
            .await
            .map_err(|error| format!("the database could not be reached: {error}"))?;
        database
            .migrate()
            .await
            .map_err(|error| format!("the schema could not be brought up to date: {error}"))?;

        let state = platform_public_api::ApiState::new(database.clone(), AUDIENCE);
        Ok((platform_public_api::routes(state), Some(database)))
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("check-config") {
        return platform_http::check_config(ROLE);
    }
    platform_http::run(ROLE, EdgeRoutes).await
}
