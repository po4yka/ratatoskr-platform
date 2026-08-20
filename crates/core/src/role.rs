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

    /// Whether this role may bind a public listener.
    ///
    /// `Edge` and, since milestone 7, `Ingest`: a webhook source reaches
    /// `POST /v1/ingest/webhooks/{source_id}` over the public internet, so the adapter
    /// `ARCHITECTURE.md` S9 describes cannot exist without a listener to reach it on.
    ///
    /// `Scheduler` is permanently false (`ARCHITECTURE.md` S18: "no public listener except
    /// health"). It publishes commands and answers to nobody.
    #[must_use]
    pub const fn may_have_public_listener(self) -> bool {
        matches!(self, Self::Edge | Self::Ingest)
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

    /// The default public listener port, for the role that has one.
    ///
    /// `Edge` only, and `Ingest` deliberately NOT — even though it may and must listen publicly.
    ///
    /// A default is a promise that the port is free, and on the deployment target that promise is
    /// false: `8081` is held by another process, so a compiled default would make `ratatoskr-ingest`
    /// crash-loop with an error about an address rather than about an allocation, and the reflexive
    /// repair is a wildcard bind that publishes the webhook surface to the whole network. A port on
    /// a host with co-tenants is an allocation
    /// (`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`), not a default, and nine more services are
    /// queued for that box.
    ///
    /// The consequence is intentional: with no default the `public` table is absent from Ingest's
    /// defaults, so validation rule V1 refuses to start it until an operator names a bind. Edge
    /// keeps its default because one role can own the obvious port and a developer running only the
    /// client-facing API should not have to configure anything.
    #[must_use]
    pub const fn default_public_port(self) -> Option<u16> {
        match self {
            Self::Edge => Some(8080),
            Self::Ingest | Self::Scheduler => None,
        }
    }
}

impl core::fmt::Display for RuntimeRole {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
