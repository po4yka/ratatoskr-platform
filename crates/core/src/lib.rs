//! Runtime role, typed platform configuration, and the platform error taxonomy.
//!
//! This crate is the part of `ratatoskr-platform` that a domain crate may depend on: it has no
//! axum, no OpenTelemetry and no HTTP server, so milestone 2's `identity` and `persistence` crates
//! can use [`PlatformError`] and [`PlatformConfig`] without linking a web framework.
//!
//! - [`role`] — [`RuntimeRole`], the deployment axis. Fixed by the binary, never read from the
//!   environment.
//! - [`config`] — the typed tree, the `RATATOSKR__` loader, and the startup rules that a process
//!   must satisfy before it binds anything.
//! - [`error`] — [`PlatformError`] and the closed public failure taxonomy the HTTP boundary
//!   projects onto a contract `ErrorEnvelope`.
//! - [`capability`] — [`Capability`], the closed vocabulary `GET /v2/capabilities` reports from.
//! - [`address`] — the one bound both routes that accept an address apply.

pub mod address;
pub mod capability;
pub mod config;
pub mod error;
pub mod role;

pub use crate::capability::{Capability, Deployment, Requirement};
pub use crate::config::{
    BusConfig, ConfigError, DatabaseConfig, IdentityConfig, PlatformConfig, Violation,
};
pub use crate::error::{FailureKind, PlatformError, PublicFault, Subsystem};
pub use crate::role::RuntimeRole;
