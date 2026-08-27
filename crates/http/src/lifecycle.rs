//! The two facts readiness is computed from at milestone 1, and the checks it reports.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};

use platform_core::RuntimeRole;
use platform_telemetry::metrics::PLATFORM_READINESS;

/// No database is configured for this role.
const DATABASE_ABSENT: u8 = 0;
/// The last probe answered.
const DATABASE_UP: u8 = 1;
/// The last probe did not answer.
const DATABASE_DOWN: u8 = 2;

/// No bus is configured for this role. Two of the three have none by design (ADR-0013).
const BUS_ABSENT: u8 = 0;
/// The client holds a connection.
const BUS_UP: u8 = 1;
/// The client is disconnected or reconnecting.
const BUS_DOWN: u8 = 2;

/// The facts readiness is computed from.
///
/// Shared by the admin router, which reads it, and the shutdown sequence, which writes it.
#[derive(Debug)]
pub struct RuntimeState {
    /// The deployable this process is. Never read from the environment.
    role: RuntimeRole,
    /// Configuration validated, telemetry installed, every configured listener bound.
    startup_complete: AtomicBool,
    /// A shutdown signal arrived.
    draining: AtomicBool,
    /// The database: 0 not configured, 1 answering, 2 not answering. Three states rather than a
    /// `bool`, because "no database" and "a database that is down" must not report the same thing.
    database: AtomicU8,
    /// Latest successful database observation in Unix microseconds, or zero before one exists.
    database_observed_at: AtomicI64,
    /// The bus, with the same three states and for the same reason. Only `ratatoskr-edge` ever
    /// leaves this at anything but `BUS_ABSENT`: it is the only role that opens a NATS connection
    /// (ADR-0013), so a bus check on the other two would report the health of something they do not
    /// use.
    bus: AtomicU8,
    /// Latest successful bus observation in Unix microseconds, or zero before one exists.
    bus_observed_at: AtomicI64,
}

impl RuntimeState {
    /// A process that has bound nothing yet: readiness fails, liveness does not.
    #[must_use]
    pub fn new(role: RuntimeRole) -> Self {
        let state = Self {
            role,
            startup_complete: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            database: AtomicU8::new(DATABASE_ABSENT),
            database_observed_at: AtomicI64::new(0),
            bus: AtomicU8::new(BUS_ABSENT),
            bus_observed_at: AtomicI64::new(0),
        };
        state.publish_readiness();
        state
    }

    /// The deployable this process is.
    #[must_use]
    pub fn role(&self) -> RuntimeRole {
        self.role
    }

    /// Every listener is bound and telemetry is up. Set exactly once.
    pub fn mark_startup_complete(&self) {
        self.startup_complete.store(true, Ordering::Release);
        self.publish_readiness();
    }

    /// Record what the latest database probe found.
    ///
    /// Called by the prober, not by a request: a readiness probe must not open a connection, or a
    /// saturated pool would make the health check the thing that finishes it off.
    pub fn set_database_reachable(&self, reachable: bool) {
        if reachable {
            self.database_observed_at
                .store(jiff::Timestamp::now().as_microsecond(), Ordering::Release);
        }
        self.database.store(
            if reachable {
                DATABASE_UP
            } else {
                DATABASE_DOWN
            },
            Ordering::Release,
        );
        self.publish_readiness();
    }

    /// Record what the latest bus probe found.
    ///
    /// The probe is `async-nats`'s own connection state, which is free to read and performs no I/O:
    /// the client reconnects on its own and tracks where it is. A probe that published a message to
    /// find out would add load to the broker in exactly the situation where the broker is already
    /// the problem.
    pub fn set_bus_reachable(&self, reachable: bool) {
        if reachable {
            self.bus_observed_at
                .store(jiff::Timestamp::now().as_microsecond(), Ordering::Release);
        }
        self.bus
            .store(if reachable { BUS_UP } else { BUS_DOWN }, Ordering::Release);
        self.publish_readiness();
    }

    /// What the last bus probe found, or `None` when this role has no bus configured.
    #[must_use]
    pub fn bus_reachable(&self) -> Option<bool> {
        match self.bus.load(Ordering::Acquire) {
            BUS_ABSENT => None,
            state => Some(state == BUS_UP),
        }
    }

    /// Latest successful bus observation, absent before the first successful probe.
    #[must_use]
    pub fn bus_observed_at(&self) -> Option<jiff::Timestamp> {
        observed_at(&self.bus_observed_at)
    }

    /// What the last database probe found, or `None` when this role has no database configured.
    ///
    /// Read by `GET /v1/capabilities` (ADR-0008) so the "healthy" half of a capability is the same
    /// fact `/health/ready` reports, arrived at the same way. A capability that consulted a
    /// different signal could disagree with readiness, and one of the two would be wrong.
    #[must_use]
    pub fn database_reachable(&self) -> Option<bool> {
        match self.database.load(Ordering::Acquire) {
            DATABASE_ABSENT => None,
            state => Some(state == DATABASE_UP),
        }
    }

    /// Latest successful database observation, absent before the first successful probe.
    #[must_use]
    pub fn database_observed_at(&self) -> Option<jiff::Timestamp> {
        observed_at(&self.database_observed_at)
    }

    /// A shutdown signal arrived. Readiness fails immediately; the listeners stay open.
    pub fn begin_draining(&self) {
        self.draining.store(true, Ordering::Release);
        self.publish_readiness();
    }

    /// The readiness checks, sorted by name.
    ///
    /// A `Vec`, never a map, so two consecutive probe bodies are byte-identical and `diff` is a
    /// usable tool at 03:00. There is deliberately no registry and no trait: a trait with one
    /// implementation is the abstraction this project rejects.
    ///
    /// "Sorted by name" became true when the bus check arrived. The sort is by [`CheckName`]'s
    /// `Ord`, which is declaration order, and the declaration order was `Drain, Startup, Database`
    /// — so the doc had described alphabetical order while the code produced the order somebody
    /// happened to add the variants in. The variants are now alphabetical and the two agree.
    #[must_use]
    pub fn checks(&self) -> Vec<Check> {
        let draining = self.draining.load(Ordering::Acquire);
        let started = self.startup_complete.load(Ordering::Acquire);
        let mut checks = vec![
            Check {
                name: CheckName::Drain,
                state: CheckState::from_pass(!draining),
                reason: draining.then_some(CheckReason::ShutdownRequested),
            },
            Check {
                name: CheckName::Startup,
                state: CheckState::from_pass(started),
                reason: (!started).then_some(CheckReason::StartupIncomplete),
            },
        ];

        match self.database.load(Ordering::Acquire) {
            DATABASE_ABSENT => {}
            state => {
                let up = state == DATABASE_UP;
                checks.push(Check {
                    name: CheckName::Database,
                    state: CheckState::from_pass(up),
                    reason: (!up).then_some(CheckReason::DependencyUnavailable),
                });
            }
        }

        match self.bus.load(Ordering::Acquire) {
            BUS_ABSENT => {}
            state => {
                let up = state == BUS_UP;
                checks.push(Check {
                    name: CheckName::Bus,
                    state: CheckState::from_pass(up),
                    reason: (!up).then_some(CheckReason::DependencyUnavailable),
                });
            }
        }

        checks.sort_unstable_by_key(|check| check.name);
        checks
    }

    /// Whether new work may be routed to this process.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.startup_complete.load(Ordering::Acquire) && !self.draining.load(Ordering::Acquire)
    }

    /// `platform_readiness{role}`, the aggregate of [`Self::checks`].
    fn publish_readiness(&self) {
        let value = if self.is_ready() { 1.0 } else { 0.0 };
        metrics::gauge!(PLATFORM_READINESS, "role" => self.role.as_str()).set(value);
    }
}

fn observed_at(storage: &AtomicI64) -> Option<jiff::Timestamp> {
    let micros = storage.load(Ordering::Acquire);
    (micros != 0)
        .then(|| jiff::Timestamp::from_microsecond(micros).ok())
        .flatten()
}

/// One readiness check.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Check {
    /// The logical name of the subject.
    pub name: CheckName,
    /// Whether the subject passes.
    pub state: CheckState,
    /// Why it does not, when it does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<CheckReason>,
}

/// A logical token from a closed set. Never a hostname, port, DSN, NATS subject, latency or driver
/// message (`ARCHITECTURE.md` S15, S12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckName {
    /// The `NATS` client holds a connection. Present only for the role that opens one, which is
    /// `ratatoskr-edge` alone (ADR-0013).
    Bus,
    /// The database answers. Present only when one is configured: a role with no database reports no
    /// database check rather than a passing one, because a passing check for something that does not
    /// exist is the readiness equivalent of an always-zero metric.
    Database,
    /// No shutdown signal has arrived.
    Drain,
    /// Configuration, telemetry and every configured listener are up.
    Startup,
}

/// Whether one check passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    /// The subject is healthy.
    Pass,
    /// The subject is not healthy.
    Fail,
}

impl CheckState {
    /// The state a boolean subject is in.
    fn from_pass(pass: bool) -> Self {
        if pass { Self::Pass } else { Self::Fail }
    }
}

/// A closed set. NEVER a formatted dependency error: a driver message can carry a host, a port, a
/// user name and sometimes a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckReason {
    /// The process has not finished binding its listeners.
    StartupIncomplete,
    /// A shutdown signal arrived and this instance is draining.
    ShutdownRequested,
    /// The last probe of the database did not answer.
    DependencyUnavailable,
}

/// The state the one public-router middleware needs.
///
/// Separate from [`RuntimeState`] because the public router is the only thing that has one: a
/// scheduler binds no public listener, so it counts no requests.
#[derive(Debug)]
pub struct HttpState {
    /// The `role` label of every `http_server_*` series this process emits.
    pub(crate) role: RuntimeRole,
    /// Requests currently inside the middleware. Read once, at close, for the shutdown log.
    pub(crate) in_flight: AtomicUsize,
}

impl HttpState {
    /// A public router that has served nothing yet.
    #[must_use]
    pub fn new(role: RuntimeRole) -> Self {
        Self {
            role,
            in_flight: AtomicUsize::new(0),
        }
    }

    /// The in-flight counter the shutdown sequence reports as `in_flight_at_close`.
    ///
    /// There is deliberately no `http_server_requests_in_flight` metric at milestone 1: the one
    /// consumer of this number is the shutdown log line, and a gauge nobody scrapes is a name to
    /// maintain for nothing. The metric arrives at milestone 5 with the first real handler.
    #[must_use]
    pub fn in_flight(&self) -> &AtomicUsize {
        &self.in_flight
    }
}
