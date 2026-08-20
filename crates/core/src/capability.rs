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
    /// `content.submit` — submitting an address for capture, at `POST /v1/captures`.
    ContentSubmit,
    /// `telegram.mini_app` — exchanging a `ratatoskr-telegram` identity assertion for a session, at
    /// `POST /v1/sessions/telegram`.
    TelegramMiniApp,
}

impl Capability {
    /// Every capability, in wire order — sorted, so two consecutive responses from an unchanged
    /// deployment are byte-identical. The array length is the documented count, so adding a variant
    /// without extending this does not compile.
    pub const ALL: [Self; 2] = [Self::ContentSubmit, Self::TelegramMiniApp];

    /// The public name. This is a contract: a client gates a feature on this exact string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentSubmit => "content.submit",
            Self::TelegramMiniApp => "telegram.mini_app",
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
            // No bus: exchanging an assertion for a session touches the database and nothing else.
            // The key is what makes it possible at all — without one, Platform cannot verify what
            // `ratatoskr-telegram` says and the route refuses everything (ADR-0011).
            Self::TelegramMiniApp => Requirement::DatabaseAndAssertionKey,
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
    /// A reachable database and a configured assertion key.
    DatabaseAndAssertionKey,
}

impl Requirement {
    /// Whether this requirement is met by `deployment`.
    #[must_use]
    pub const fn is_met(self, deployment: &Deployment) -> bool {
        match self {
            Self::Database => deployment.database_reachable,
            Self::DatabaseAndBus => deployment.database_reachable && deployment.bus_configured,
            Self::DatabaseAndAssertionKey => {
                deployment.database_reachable && deployment.assertion_key_configured
            }
        }
    }
}

/// What this deployment has, as far as a capability is concerned.
///
/// A struct rather than a growing argument list, because every capability added since has needed a
/// different subset and a positional `bool` pair was already one rename away from being silently
/// swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Deployment {
    /// What the readiness prober last found — never a fresh probe. A public request must not be the
    /// thing that opens a connection in order to decide whether connections can be opened.
    pub database_reachable: bool,
    /// Whether an event bus is configured at all. Not "is the broker up": a bus that is configured
    /// and briefly unreachable still publishes the outbox when it returns, whereas a deployment
    /// with none never will.
    pub bus_configured: bool,
    /// Whether an assertion verification key is configured.
    pub assertion_key_configured: bool,
}
