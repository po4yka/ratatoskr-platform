//! Typed configuration: the tree, the loader, and the startup rules.
//!
//! # Sources and precedence
//!
//! Two providers, lowest precedence first:
//!
//! 1. built-in defaults for the [`RuntimeRole`] — the `public` table is present for `Edge` and
//!    absent from the other two roles, which is what makes every binary run in an empty
//!    environment;
//! 2. `RATATOSKR__` environment variables, with `__` separating nesting levels, e.g.
//!    `RATATOSKR__TELEMETRY__OTLP__ENDPOINT`.
//!
//! # There is deliberately no configuration file at milestone 1
//!
//! One mechanism and one place to look. No search path, no provenance check, no rule that a secret
//! may not come from a file — none of which can be wrong if there is no file. The deployment model
//! is a container reading a `ConfigMap` into its environment. A lower-precedence file provider is a
//! backward-compatible one-line addition at the milestone where an operator asks for one.
//!
//! What is not deferrable is the naming scheme: environment variable names are an operational
//! contract, and renaming them later breaks every deployment manifest in the fleet.

mod model;
mod validate;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use figment::Figment;
use figment::providers::{Env, Serialized};

pub use crate::config::model::{
    AdminConfig, BusConfig, DatabaseConfig, LogFormat, OtlpConfig, PlatformConfig, PublicConfig,
    ShutdownConfig, TelemetryConfig,
};
pub use crate::config::validate::Violation;
use crate::role::RuntimeRole;

/// The environment prefix, and the nesting separator inside it.
const ENV_PREFIX: &str = "RATATOSKR__";

/// The default public listener address of the edge role.
const DEFAULT_PUBLIC_PORT: u16 = 8080;

/// Reads the process environment and produces a validated configuration for `role`.
///
/// Sources, lowest precedence first: built-in defaults for `role`, then `RATATOSKR__` environment
/// variables with `__` separating nesting levels. There is no configuration file (see the module
/// documentation for why).
///
/// # Errors
///
/// [`ConfigError::Source`] when extraction fails — a wrong type or an unknown key. figment is
/// fail-fast, so this reports exactly one problem, and it names both the key and the provider.
/// [`ConfigError::Invalid`] carrying EVERY semantic violation found, never only the first, because
/// an operator editing a `ConfigMap` wants one round trip and not five.
#[allow(
    clippy::result_large_err,
    reason = "figment::Error is the specified payload of ConfigError::Source; boxing it would hide \
              the key and provider it names behind an extra indirection for a value that is \
              constructed once, at startup, on the path that then exits"
)]
pub fn load(role: RuntimeRole) -> Result<PlatformConfig, ConfigError> {
    load_from(role, figment(role))
}

/// The provider stack [`load`] uses, exposed so a test can add a provider on top of it.
#[must_use]
pub fn figment(role: RuntimeRole) -> Figment {
    Figment::from(Serialized::defaults(PlatformConfig::defaults(role)))
        .merge(Env::prefixed(ENV_PREFIX).split("__"))
}

/// Extracts and validates from an arbitrary figment. The seam every configuration test uses.
///
/// # Errors
///
/// As [`load`].
#[allow(
    clippy::result_large_err,
    reason = "as `load`: the error payload is figment::Error by specification"
)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "the figment is consumed so a caller cannot extract the same stack twice and reason \
              about two configurations in one process"
)]
pub fn load_from(role: RuntimeRole, figment: Figment) -> Result<PlatformConfig, ConfigError> {
    let config: PlatformConfig = figment.extract()?;
    let violations = validate::validate(role, &config);
    if violations.is_empty() {
        Ok(config)
    } else {
        Err(ConfigError::Invalid(violations))
    }
}

impl PlatformConfig {
    /// The built-in defaults for `role`. The ONLY place a default value is written, and the source
    /// of the table in `.env.example`.
    #[must_use]
    pub fn defaults(role: RuntimeRole) -> Self {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        Self {
            admin: AdminConfig {
                bind: SocketAddr::new(loopback, role.default_admin_port()),
            },
            // Absent by default, in every role. A database URL carries a credential, so there is
            // no default that is not either wrong or a secret in the source tree. Milestone 5 makes
            // it required for the roles that serve data; until then a process without one starts and
            // reports no database check.
            database: None,
            // Absent by default. A broker URL is deployment topology, and a default would be wrong
            // everywhere except one laptop.
            bus: None,
            // Role-aware: the table is present for the one role that may serve public traffic and
            // absent from the others, so rule V1 is satisfied by the defaults alone.
            public: role.may_have_public_listener().then(|| PublicConfig {
                bind: SocketAddr::new(loopback, DEFAULT_PUBLIC_PORT),
                request_timeout_seconds: model::default_request_timeout_seconds(),
                max_body_bytes: model::default_max_body_bytes(),
            }),
            shutdown: ShutdownConfig {
                drain_seconds: model::default_drain_seconds(),
                grace_seconds: model::default_grace_seconds(),
            },
            telemetry: TelemetryConfig {
                log_format: LogFormat::default(),
                log_filter: model::default_log_filter(),
                otlp: None,
            },
        }
    }
}

/// Every reason a Platform process must refuse to start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// Extraction failed: a wrong type, a missing key, or an unknown key. figment names both the
    /// key and the provider it came from, and is fail-fast, so this carries exactly one problem.
    ///
    /// figment's own message is deliberately NOT interpolated. It quotes the supplied value for
    /// several field types (`invalid type: found string "…", expected u64`;
    /// ``unknown variant: found `…` ``), so a `Display` that carried it would make one
    /// `tracing::error!(%error)` — on a tree that holds a `DATABASE_URL` from milestone 2 — a live
    /// secret leak. [`ConfigError::report`] is the only operator-facing rendering, and it is
    /// value-free by construction. This is the sibling of the `Violation` rationale: safety as a
    /// type property, not a rule someone has to remember. (This narrows the specification's
    /// `{0}`; §3.6 fixed the string before that consequence was noticed.)
    #[error("configuration could not be read")]
    Source(#[from] figment::Error),

    /// The configuration parsed but violates one or more startup rules. Carries every violation
    /// found.
    #[error("configuration is invalid: {} problem(s)", .0.len())]
    Invalid(Vec<Violation>),
}

impl ConfigError {
    /// The operator-facing report, written to stderr before any subscriber exists.
    /// One block per problem, stable order, no supplied values.
    #[must_use]
    pub fn report(&self, role: RuntimeRole) -> String {
        match self {
            Self::Source(error) => validate::report_unreadable(role, error),
            Self::Invalid(violations) => validate::report_invalid(role, violations),
        }
    }

    /// `78` — `EX_CONFIG` from `sysexits.h`. Kubernetes surfaces it in
    /// `lastState.terminated.exitCode`, which is what distinguishes "your configuration is wrong"
    /// from "the process crashed" in a restart-loop dashboard.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        78
    }
}
