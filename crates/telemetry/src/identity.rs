//! The one wire identity of this bounded context, and the OpenTelemetry resource built from it.

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use platform_core::RuntimeRole;

/// The one wire identity of this bounded context, and the OpenTelemetry `service.name`.
///
/// ONE identity for all three binaries (ADR-0003). `contracts.toml [services].known` lists exactly
/// this token for this repository, and all four `[[contract]]` entries name it as owner, producer
/// and consumer.
///
/// Observability and wire identity read the same constant on purpose, so they cannot drift.
pub const SERVICE_NAME: &str = "ratatoskr-platform";

/// The crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Rust toolchain the binary was built with, from the workspace `rust-version`.
/// Test X-4 asserts it is the prefix of `rust-toolchain.toml`'s channel, so the two cannot drift.
pub const RUST_VERSION: &str = env!("CARGO_PKG_RUST_VERSION");

/// The build's git commit, supplied by the container build as `RATATOSKR_GIT_SHA`; `"unknown"`
/// otherwise. Deliberately not a `build.rs` shelling out to `git`: a Docker build has no `.git`, so
/// that approach returns `"unknown"` exactly where the answer matters.
pub const GIT_SHA: &str = match option_env!("RATATOSKR_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

/// The OpenTelemetry semantic-convention key `service.name`.
///
/// The three semantic-convention keys this crate uses are three string constants and live here
/// rather than in an `opentelemetry-semantic-conventions` dependency that contributes nothing else.
const SERVICE_NAME_KEY: &str = "service.name";

/// The OpenTelemetry semantic-convention key `service.namespace`.
const SERVICE_NAMESPACE_KEY: &str = "service.namespace";

/// The OpenTelemetry semantic-convention key `service.version`.
const SERVICE_VERSION_KEY: &str = "service.version";

/// The Ratatoskr resource attribute every dashboard facets on.
const RUNTIME_ROLE_KEY: &str = "ratatoskr.runtime_role";

/// The service namespace all Ratatoskr bounded contexts share.
const SERVICE_NAMESPACE: &str = "ratatoskr";

/// The OpenTelemetry resource for `role`.
///
/// | attribute | value |
/// |---|---|
/// | `service.name` | [`SERVICE_NAME`] |
/// | `service.namespace` | `ratatoskr` |
/// | `service.version` | [`VERSION`] |
/// | `ratatoskr.runtime_role` | `edge` \| `ingest` \| `scheduler` |
///
/// Dashboards facet on `ratatoskr.runtime_role`, never on `service.name`. There is deliberately no
/// `deployment.environment.name`: a scrape or collector configuration is the standard place for it
/// (Prometheus `external_labels`, the OpenTelemetry collector `resource` processor), and Platform
/// adding a config field for something the platform already does is a field for nothing.
#[must_use]
pub fn resource(role: RuntimeRole) -> Resource {
    // `with_attributes` merges last-wins, so these override anything the environment detector that
    // `Resource::builder` runs may have supplied.
    Resource::builder()
        .with_attributes([
            KeyValue::new(SERVICE_NAME_KEY, SERVICE_NAME),
            KeyValue::new(SERVICE_NAMESPACE_KEY, SERVICE_NAMESPACE),
            KeyValue::new(SERVICE_VERSION_KEY, VERSION),
            KeyValue::new(RUNTIME_ROLE_KEY, role.as_str()),
        ])
        .build()
}
