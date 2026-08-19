//! The closed vocabulary of public capabilities.
//!
//! `ARCHITECTURE.md` S12: "Capabilities reflect enabled, healthy, and authorized features. They do
//! not reveal internal service topology or secrets." S19 invariant 12: they describe supported
//! public behaviour, not topology.
//!
//! ADR-0008 fixes what that means. A capability is a variant of this enum, it names a route family
//! this build actually serves, and it is reported when its deployment requirement is configured,
//! the components that requirement names are healthy, and the caller is authorized for it.
//!
//! S12 prints six example names. Five of them describe features Platform serves no route for, so
//! they are deliberately absent: a name on this list is a promise the route tree has to keep.

/// A public capability, and the whole vocabulary of them.
///
/// An enum rather than a string so the set is closed at compile time: the response body is drawn
/// from a fixed collection of literals and can never carry an operator-supplied value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// `content.submit` — submitting an address for capture, at `POST /v2/captures`.
    ContentSubmit,
}

impl Capability {
    /// Every capability, in wire order. The array length is the documented count, so adding a
    /// variant without extending this does not compile.
    pub const ALL: [Self; 1] = [Self::ContentSubmit];

    /// The public name. This is a contract: a client gates a feature on this exact string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentSubmit => "content.submit",
        }
    }

    /// What this deployment must have configured and healthy for the capability to work end to end
    /// from the caller's side.
    #[must_use]
    pub const fn requires(self) -> Requirement {
        match self {
            // Not only the database. A capture is accepted durably with no bus configured — the
            // outbox is the durable half — but the command is then never published and the
            // operation never progresses. From the client's side that is not a working feature.
            Self::ContentSubmit => Requirement::DatabaseAndBus,
        }
    }
}

impl core::fmt::Display for Capability {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a capability needs from the deployment it runs in.
///
/// Two variants, because two are what the vocabulary distinguishes today. A third arrives with the
/// first capability that needs something else, not before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Requirement {
    /// A reachable database.
    Database,
    /// A reachable database and a configured bus.
    DatabaseAndBus,
}

impl Requirement {
    /// Whether this requirement is met.
    ///
    /// `database_reachable` is the answer the readiness prober last recorded, not a fresh probe:
    /// a public request must never be the thing that opens a connection to decide whether
    /// connections can be opened.
    #[must_use]
    pub const fn is_met(self, database_reachable: bool, bus_configured: bool) -> bool {
        match self {
            Self::Database => database_reachable,
            Self::DatabaseAndBus => database_reachable && bus_configured,
        }
    }
}
