//! The `ratatoskr-scheduler` deployable.
//!
//! Milestone 9, and the first milestone at which this binary does anything. It publishes the
//! periodic commands of `ARCHITECTURE.md` S10: a schedule row becomes a deterministic occurrence,
//! an operation and an outbox command, all in one transaction.
//!
//! # Three things it deliberately does not do
//!
//! **It does not listen.** S18: "no public listener except health". Rule V1 refuses a public bind
//! for this role permanently, so the only socket is the operator listener.
//!
//! **It does not create the schema.** S18 gives it its own least-privilege database role, and a role
//! that may create a table is not one. `ratatoskr-edge` owns `schema.sql`; this process checks that
//! it has been applied and refuses to start if it has not.
//!
//! **It does not reach the broker.** The command is written to `operations.outbox` and stops there;
//! `ratatoskr-edge` moves it to NATS. That is one pump on one host, decided in ADR-0013 rather than
//! deferred: a scheduler that cannot reach the broker still records its occurrence durably, and the
//! consequence — no edge means no command leaves the machine — is a backlog an operator can see
//! rather than an occurrence nobody kept. `RATATOSKR__BUS__URL` is therefore not read by this
//! binary at all.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use platform_core::RuntimeRole;
use platform_core::config::PlatformConfig;
use platform_http::{RuntimeState, Serving};
use platform_persistence::Database;
use sqlx::PgPool;

const ROLE: RuntimeRole = RuntimeRole::Scheduler;

/// How often the publisher looks for due schedules.
///
/// One second, which is the floor this process puts under `platform_scheduler_drift_seconds`. The
/// shortest legal interval is a minute (`schedules_interval_is_between_a_minute_and_a_year`), so a
/// second of granularity is under two percent of the fastest schedule anyone can write, and the
/// query behind it is one partial-index lookup that usually returns nothing.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// How many schedules one pass handles. A bound, so one pass cannot become unbounded work — the
/// same reason the outbox pump claims a batch rather than everything due.
const TICK_BATCH: i64 = 32;

/// Scheduler's contribution, which is a background loop and no routes.
struct SchedulerLoop;

impl platform_http::PublicRoutes for SchedulerLoop {
    async fn build(
        self,
        config: &PlatformConfig,
        _health: &Arc<RuntimeState>,
    ) -> Result<Serving, String> {
        let Some(database) = config.database.as_ref() else {
            return Err(
                "ratatoskr-scheduler publishes commands from stored schedules and requires \
                 RATATOSKR__DATABASE__URL"
                    .to_owned(),
            );
        };

        let database = Database::connect(database)
            .await
            .map_err(|error| format!("the database could not be reached: {error}"))?;

        let present = platform_scheduling::schema_is_present(database.pool())
            .await
            .map_err(|error| format!("the schema could not be inspected: {error}"))?;
        if !present {
            return Err(
                "operations.schedules is absent; ratatoskr-edge applies schema.sql and must \
                 have run at least once against this database"
                    .to_owned(),
            );
        }

        announce_schedules(database.pool()).await;

        Ok(Serving {
            routes: axum::Router::new(),
            database: Some(database.clone()),
            tasks: vec![spawn_publisher(database.pool().clone())],
        })
    }
}

/// Say at startup how much work this process has, because the alternative is a silent process that
/// looks identical whether it has twelve schedules or none.
async fn announce_schedules(pool: &PgPool) {
    match sqlx::query_scalar::<_, i64>("select count(*) from operations.schedules where enabled")
        .fetch_one(pool)
        .await
    {
        Ok(0) => tracing::warn!(
            "no schedule is enabled; this process will publish nothing until one is. A schedule \
             is created disabled on purpose (deploy/README.md)"
        ),
        Ok(enabled) => tracing::info!(enabled, "enabled schedules"),
        Err(error) => tracing::warn!(%error, "the schedules could not be counted"),
    }
}

/// Publish due occurrences, forever.
fn spawn_publisher(pool: PgPool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK_INTERVAL);
        // Delay rather than Burst: a pass that took longer than a tick must not be followed by a
        // storm of catch-up ticks, which would claim the same schedules over and over.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match platform_scheduling::run_once(&pool, TICK_BATCH, jiff::Timestamp::now()).await {
                Ok(report) if report.due > 0 => {
                    tracing::info!(
                        due = report.due,
                        published = report.published,
                        suppressed = report.suppressed,
                        skipped = report.skipped,
                        failed = report.failed,
                        "schedule pass",
                    );
                }
                Ok(_) => {}
                // The due query failed, which means the database did. The readiness prober is what
                // reports that; this loop simply tries again on the next tick.
                Err(error) => tracing::warn!(%error, "a schedule pass failed"),
            }
        }
    })
}

/// A single-threaded runtime.
///
/// The multi-threaded one starts a worker per core and keeps a blocking pool that grows to 512
/// threads. This process has one background loop that sleeps for a second at a time, four operator
/// routes nobody polls hard, and a connection pool of ten; there is nothing here for a second
/// worker to do. On four cores shared with `PostgreSQL`, NATS and a metrics stack, a runtime sized
/// from `num_cpus` is the default `AGENTS.md` calls a bug on this host.
///
/// The rule this buys into: nothing in this binary may block the thread. Every I/O it performs is
/// `sqlx` over Tokio, and the one synchronous file read in the workspace —
/// `NatsPublisher::connect_with_nkey` — is in a binary this one does not share code with.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("check-config") {
        return platform_http::check_config(ROLE);
    }
    platform_http::run(ROLE, SchedulerLoop).await
}
