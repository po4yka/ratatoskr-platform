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

use jiff::SignedDuration;
use platform_core::RuntimeRole;
use platform_core::config::{PlatformConfig, RetentionConfig};
use platform_eventing::{
    COMMAND_STREAM, EDGE_PROJECTION_CONSUMER, NatsPublisher, StreamSpec, pump,
};
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
///
/// One second, raised from 250 ms with the deployment profile. The four-fold reduction is not the
/// point — the point is that this loop runs forever on four shared cores next to `PostgreSQL`, NATS
/// and a metrics stack, and a quarter-second poll of an empty queue is the shape of default that
/// `AGENTS.md` calls a bug on this host. What it costs is up to a second of latency between
/// accepting a capture and putting it on the bus, on a path whose whole contract is that the API
/// acknowledges durable ACCEPTANCE and not completion.
const PUMP_INTERVAL: Duration = Duration::from_secs(1);

/// How many rows one pass claims. A bound, so one slow broker cannot make one pass unbounded.
const PUMP_BATCH: i64 = 64;

/// How often the gauges that are properties of a SET rather than of an event are sampled.
///
/// Fifteen seconds, which is the scrape interval `deploy/monitoring/promscrape.ratatoskr.yml`
/// configures. Sampling faster produces points nobody reads; sampling slower produces a scrape that
/// reads the same point twice and a graph that steps.
///
/// Each tick runs three aggregates. They are the only reason this loop exists: a queue depth, an
/// operation age and a capability are not knowable from any single write, so publishing them from
/// the write path would put a full scan on a request to keep a gauge fresh between scrapes.
const OBSERVE_INTERVAL: Duration = Duration::from_secs(15);

/// How often the retention sweep runs.
///
/// Hourly. Every window it enforces is measured in days, so a sweep an hour late is invisible; what
/// the interval really bounds is how much a table can grow between two chances to shrink it, and an
/// hour of the fastest producer here is a few thousand rows.
const RETENTION_INTERVAL: Duration = Duration::from_hours(1);

/// How many rows one sweep removes from one table.
///
/// A bound rather than a target. An unbounded `delete` against a table nobody has pruned since the
/// deployment started takes a lock proportional to the neglect, on a database three services share.
/// A backlog therefore drains over successive hours; the sweep logs what it removed, so a count
/// pinned at this number is the signal that it is still draining.
const RETENTION_BATCH: i64 = 10_000;

/// How often the bus connection state is copied into readiness.
///
/// Five seconds, matching the database prober. The read itself is free — `async-nats` tracks its
/// own connection and this asks it — so the interval is chosen to bound how stale `/health/ready`
/// can be, not to bound cost.
const BUS_PROBE_INTERVAL: Duration = Duration::from_secs(5);

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
        // An anonymous connection is legal and is what `compose.yaml` serves; a deployment sets
        // `RATATOSKR__BUS__NKEY_SEED_PATH` so that this process may publish `cmd.>` and consume
        // `evt.>` and nothing else (`deploy/nats/ratatoskr.conf`). Rule V16 has already checked the
        // file is there, so reaching the error path here means it went away between validation and
        // now.
        let publisher = match bus.nkey_seed_path.as_deref() {
            Some(seed) => NatsPublisher::connect_with_nkey(bus.url.as_str(), seed).await,
            None => NatsPublisher::connect(bus.url.as_str()).await,
        }
        .map_err(|error| format!("the bus could not be reached: {error}"))?;
        // The names and limits are the deployment profile's, stated once in
        // `platform_eventing::stream` so that this process, the NATS permission file and the
        // operator commands in `deploy/README.md` cannot disagree about them.
        let command_stream = StreamSpec::command_stream();
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
        // The constructor's default exists so a test can build a state without a configuration; a
        // deployment states its own allowance, and rule V18 has already refused a zero.
        if let Some(public) = config.public.as_ref() {
            state.actor_limit = Arc::new(platform_http::ActorLimiter::new(
                public.actor_requests_per_minute,
            ));
        }

        // Shared with the observer rather than moved into the router, so the capability gauges are
        // computed from the SAME state the route reports (ADR-0008: one source for that fact, or
        // the two disagree and one of them is wrong).
        let state = Arc::new(state);
        let tasks = vec![
            spawn_publisher(database.pool().clone(), publisher.clone()),
            spawn_projection(database.pool().clone(), publisher.clone()),
            spawn_bus_prober(publisher, Arc::clone(health)),
            spawn_observer(database.pool().clone(), Arc::clone(&state)),
            spawn_retention(database.pool().clone(), config.retention.clone()),
        ];

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

/// Copy the bus connection state into readiness, forever.
///
/// A separate loop from the observer because it answers a different question and at a different
/// cadence: this one is a free read that keeps `/health/ready` honest, and the observer runs
/// aggregates against the database.
///
/// Like the database check, a failing bus check is REPORTED and does not by itself make the process
/// unready — `RuntimeState::is_ready` is startup and drain. That is the existing convention rather
/// than a new decision: a dependency that flaps would otherwise flap the readiness of a process
/// that is still accepting work correctly, because the outbox is the durable half and a capture is
/// accepted with the broker down.
fn spawn_bus_prober(
    publisher: NatsPublisher,
    health: Arc<RuntimeState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(BUS_PROBE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            health.set_bus_reachable(publisher.is_connected());
        }
    })
}

/// Sample the gauges that are properties of a set, forever.
///
/// Every failure here is logged and the tick continues. A sample that could not be taken is a gap
/// in a series; stopping the loop because one aggregate failed would turn that gap into a
/// permanent silence, which reads on a dashboard exactly like a healthy zero.
fn spawn_observer(
    pool: PgPool,
    state: Arc<platform_public_api::ApiState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(OBSERVE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let now = jiff::Timestamp::now();
            platform_public_api::capabilities::sample(&state);
            if let Err(error) = platform_eventing::observe::sample(&pool, now).await {
                tracing::warn!(%error, "the outbox and inbox gauges could not be sampled");
            }
            if let Err(error) = platform_operations::sample(&pool, now).await {
                tracing::warn!(%error, "the operation gauges could not be sampled");
            }
        }
    })
}

/// Delete what the retention windows say may go, forever.
///
/// It runs here for the same reason the publisher does: `ratatoskr-edge` is the process that owns
/// the database (ADR-0013), and a sweep in each of three processes would be three services deleting
/// each other's rows on three schedules.
///
/// **`operations.operations` is deliberately untouched.** Everything this removes is a record the
/// SYSTEM wrote for its own correctness — a deduplication marker, a delivered message, a security
/// decision, an occurrence — and removing one changes nothing a person can see. Operation history
/// is the opposite: it is what a user reads at `GET /v2/operations/{id}`, so how long it is kept is
/// a product decision with somebody on the other end of it, and no milestone owns that decision.
/// `DEVELOPMENT.md` records it as open rather than leaving it to be discovered.
///
/// A failure in one table does not stop the others. They are independent windows over independent
/// tables, and a sweep that abandoned the rest because the audit table was locked would let four
/// tables grow to fix one.
fn spawn_retention(pool: PgPool, windows: RetentionConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RETENTION_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            sweep(&pool, &windows).await;
        }
    })
}

/// One retention pass. Split out so the loop above stays readable and this stays testable by eye.
async fn sweep(pool: &PgPool, windows: &RetentionConfig) {
    let now = jiff::Timestamp::now();
    // Saturating and never panicking: a window an operator set to something absurd produces an
    // instant at the edge of the representable range, which deletes nothing, rather than an
    // arithmetic panic inside a background loop nobody is watching.
    let before = |days: u64| {
        now.saturating_sub(SignedDuration::from_hours(
            24_i64.saturating_mul(i64::try_from(days).unwrap_or(i64::MAX / 24)),
        ))
        .unwrap_or(jiff::Timestamp::MIN)
    };

    // Two of these need no window at all: both tables carry their own expiry, written by whoever
    // created the row, so "expired" is a fact rather than a policy.
    let idempotency = platform_idempotency::collect_expired(pool, now)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "the idempotency ledger could not be collected");
            0
        });
    let relays = platform_identity::relay::collect_expired(pool, now)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "the OAuth relays could not be collected");
            0
        });

    let inbox = platform_eventing::Inbox::collect_processed(
        pool,
        before(windows.inbox_days),
        RETENTION_BATCH,
    )
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "the inbox could not be collected");
        0
    });
    let outbox = platform_eventing::Outbox::collect_published(
        pool,
        before(windows.outbox_days),
        RETENTION_BATCH,
    )
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "the outbox could not be collected");
        0
    });
    let audit =
        platform_identity::audit::collect_before(pool, before(windows.audit_days), RETENTION_BATCH)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the audit trail could not be collected");
                0
            });
    let occurrences = platform_scheduling::collect_occurrences_before(
        pool,
        before(windows.schedule_occurrence_days),
        RETENTION_BATCH,
    )
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "the schedule occurrences could not be collected");
        0
    });

    let removed = idempotency + relays + inbox + outbox + audit + occurrences;
    if removed > 0 {
        tracing::info!(
            idempotency,
            relays,
            inbox,
            outbox,
            audit,
            occurrences,
            "retention sweep",
        );
    }
}

/// Apply inbound progress events to the operation projection, forever.
fn spawn_projection(pool: PgPool, publisher: NatsPublisher) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let outcome = platform_eventing::consumer::run(
            publisher.context(),
            &StreamSpec::event_stream(),
            EDGE_PROJECTION_CONSUMER,
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
