//! The `ratatoskr-edge` deployable.
//!
//! Milestones 1 to 7: typed configuration, telemetry, the operator listener, the public API, the
//! outbox publisher, the operation-event consumer and the capability document.
//!
//! It also owns the migrations. `ratatoskr-ingest` reads two of the schemas and applies none of
//! them (`ARCHITECTURE.md` S18: separate, least-privilege database roles), so this is the one
//! process that brings a database up to date.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_eventing::{NatsPublisher, StreamSpec, pump};
use platform_http::{RuntimeState, Serving};
use platform_operations::ProgressProjection;
use platform_persistence::Database;
use sqlx::PgPool;

const ROLE: RuntimeRole = RuntimeRole::Edge;

/// The audience a session must carry to authenticate here.
///
/// Fixed by the binary, not read from configuration: an operator who could retarget the audience
/// could make a token minted for another surface valid at this one.
const AUDIENCE: &str = "edge";

/// How often the publisher looks for due outbox rows.
///
/// `ARCHITECTURE.md` S8.2 expresses backoff by moving a row's next attempt forward, so this is only
/// how often the queue is inspected, not how fast a failed message is retried.
const PUMP_INTERVAL: Duration = Duration::from_millis(250);

/// How many rows one pass claims. A bound, so one slow broker cannot make one pass unbounded.
const PUMP_BATCH: i64 = 64;

/// The `JetStream` stream and durable consumer this role reads operation events from.
const EVENT_STREAM: &str = "ratatoskr_events";
const EVENT_CONSUMER: &str = "platform_edge_projection";

/// The `JetStream` stream commands are published to.
///
/// Declared here because this process publishes to it, and there is one of it (ADR-0010).
/// `JetStream` does not acknowledge a publish to a subject no stream covers, so without this every
/// command would be retried, backed off and eventually dead-lettered.
const COMMAND_STREAM: &str = "ratatoskr_commands";

/// The two streams, with the limits they are created with.
///
/// The asymmetry is the decision: a command stream REFUSES a publish when full, because the outbox
/// is the durable copy and a refusal becomes a visible retry; an event stream drops its oldest,
/// because an event is a fact its producer already recorded and a consumer that far behind is not
/// helped by keeping more of them.
fn command_stream() -> StreamSpec {
    StreamSpec::commands(COMMAND_STREAM, vec!["cmd.>".to_owned()])
}

fn event_stream() -> StreamSpec {
    StreamSpec::events(EVENT_STREAM, vec!["evt.>".to_owned()])
}

/// Edge's contribution to its public listener.
struct EdgeRoutes;

impl platform_http::PublicRoutes for EdgeRoutes {
    /// Connect, migrate, build the routes, and start the two background loops.
    ///
    /// Refusing to start without a database is deliberate: every route this binary serves reads or
    /// writes one, and a process that started anyway would report itself ready and then fail every
    /// request. The bus is treated the same way — the outbox is the durable half, but a publisher
    /// that cannot reach the broker means commands accumulate silently, which is worse to discover
    /// later than at startup.
    async fn build(
        self,
        config: &PlatformConfig,
        health: &Arc<RuntimeState>,
    ) -> Result<Serving, String> {
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

        // A missing bus is fatal, exactly as a missing database is. It was a warning until
        // milestone 7's survey pointed out what that bought: edge came up healthy, reported
        // `content.submit` unavailable through `/v2/capabilities`, and piled every accepted capture
        // into `operations.outbox` forever with no publisher and no alert — a silently useless
        // service that passes its own readiness check. Refusing to start is the same answer this
        // file already gives to a missing database twenty lines above, and for the same reason.
        let Some(bus) = config.bus.as_ref() else {
            return Err(
                "ratatoskr-edge publishes the outbox and consumes operation events, and requires \
                 RATATOSKR__BUS__URL"
                    .to_owned(),
            );
        };
        let publisher = NatsPublisher::connect(bus.url.as_str())
            .await
            .map_err(|error| format!("the bus could not be reached: {error}"))?;
        let command_stream = command_stream();
        let state = publisher
            .ensure_stream(&command_stream)
            .await
            .map_err(|error| format!("the command stream could not be declared: {error}"))?;
        // A stream that already exists keeps the limits it was created with, and the client says
        // nothing about it. A deployment carrying corrected limits would otherwise report success
        // and change nothing — which for a command stream means `DiscardPolicy::Old` quietly
        // deleting commands a client was told had been accepted.
        if let platform_eventing::StreamState::Existing { mismatches } = state
            && !mismatches.is_empty()
        {
            tracing::warn!(
                stream = COMMAND_STREAM,
                mismatches = ?mismatches,
                "the command stream on the broker was created with different limits and was NOT \
                 reconciled; update or delete it on the broker"
            );
        }
        let tasks = vec![
            spawn_publisher(database.pool().clone(), publisher.clone()),
            spawn_projection(database.pool().clone(), publisher),
        ];

        let mut state = platform_public_api::ApiState::new(
            database.clone(),
            AUDIENCE,
            Arc::clone(health),
            // Always true now that a bus is required to start. Kept as a parameter rather than
            // folded away because `ApiState` is also built by tests that exercise the
            // bus-less capability document, and because it is the honest shape of the question
            // `GET /v2/capabilities` asks.
            true,
        );
        // Decoded once, here, rather than on every request. Rule V14 already refused a key that is
        // not 32 bytes, so this cannot fail on a configuration the process started with — and if it
        // somehow did, refusing to start is right: a route that verifies nothing would accept
        // nothing, silently, for the life of the deployment.
        if let Some(key) = config.identity.assertion_key.as_deref() {
            let decoded =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key)
                    .map_err(|error| format!("the assertion key could not be decoded: {error}"))?;
            state.assertion_key = Some(decoded);
        }
        state
            .oauth_completion_url
            .clone_from(&config.identity.oauth_completion_url);
        Ok(Serving {
            routes: platform_public_api::routes(state),
            database: Some(database),
            tasks,
        })
    }
}

/// Move due outbox rows onto the bus, forever.
fn spawn_publisher(pool: PgPool, publisher: NatsPublisher) -> tokio::task::JoinHandle<()> {
    let name = format!("edge-{}", uuid::Uuid::now_v7());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PUMP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match pump::run_once(&pool, &publisher, &name, PUMP_BATCH, jiff::Timestamp::now()).await
            {
                Ok(report) if report.claimed > 0 => {
                    tracing::debug!(
                        published = report.published,
                        failed = report.failed,
                        dead_lettered = report.dead_lettered,
                        "outbox pass",
                    );
                }
                Ok(_) => {}
                // Claiming failed, which means the database did. The readiness prober is what
                // reports that; this loop simply tries again on the next tick.
                Err(error) => tracing::warn!(%error, "an outbox pass failed"),
            }
        }
    })
}

/// Apply inbound progress events to the operation projection, forever.
fn spawn_projection(pool: PgPool, publisher: NatsPublisher) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let outcome = platform_eventing::consumer::run(
            publisher.context(),
            &event_stream(),
            EVENT_CONSUMER,
            &pool,
            &ProgressProjection,
            std::future::pending::<()>(),
        )
        .await;
        if let Err(error) = outcome {
            tracing::error!(%error, "the operation-event consumer stopped");
        }
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("check-config") {
        return platform_http::check_config(ROLE);
    }
    platform_http::run(ROLE, EdgeRoutes).await
}
