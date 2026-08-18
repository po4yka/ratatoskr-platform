//! Which deployable this process is.

/// Which deployable this process is.
///
/// Fixed by the binary at compile time and never read from the environment: a role that could be
/// misconfigured would make a process lie in every metric it emits and would let an operator start
/// `ratatoskr-scheduler` in the edge role.
///
/// This is a DEPLOYMENT axis (`ARCHITECTURE.md` S18: separate network exposure, database roles and
/// NATS credentials). It is **not** a wire identity. See ADR-0003.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeRole {
    /// `ratatoskr-edge` — the public HTTP API.
    Edge,
    /// `ratatoskr-ingest` — generic ingress normalization.
    Ingest,
    /// `ratatoskr-scheduler` — periodic command publication.
    Scheduler,
}

impl RuntimeRole {
    /// Every role, so the `role` telemetry label can never become unbounded. The array length is
    /// the documented count, so adding a variant without updating it does not compile.
    pub const ALL: [Self; 3] = [Self::Edge, Self::Ingest, Self::Scheduler];

    /// The telemetry label and health-body value: `edge` | `ingest` | `scheduler`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Ingest => "ingest",
            Self::Scheduler => "scheduler",
        }
    }

    /// `ratatoskr-edge` | `ratatoskr-ingest` | `ratatoskr-scheduler`.
    #[must_use]
    pub const fn binary_name(self) -> &'static str {
        match self {
            Self::Edge => "ratatoskr-edge",
            Self::Ingest => "ratatoskr-ingest",
            Self::Scheduler => "ratatoskr-scheduler",
        }
    }

    /// Whether this role may bind a public listener at milestone 1.
    ///
    /// `Edge` only. `Scheduler` is permanently false (`ARCHITECTURE.md` S18: "no public listener
    /// except health"). `Ingest` becomes true at milestone 7, in the pull request that adds the
    /// first inbound adapter (`ARCHITECTURE.md` S9) — one line, reviewed where it belongs.
    #[must_use]
    pub const fn may_have_public_listener(self) -> bool {
        matches!(self, Self::Edge)
    }

    /// Distinct per role so all three binaries run on one developer machine with no configuration:
    /// `9464` | `9465` | `9466`.
    #[must_use]
    pub const fn default_admin_port(self) -> u16 {
        match self {
            Self::Edge => 9464,
            Self::Ingest => 9465,
            Self::Scheduler => 9466,
        }
    }
}

impl core::fmt::Display for RuntimeRole {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
