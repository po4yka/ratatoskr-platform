//! The typed configuration tree.
//!
//! One shape for all three roles. Role-specific requirements are validation rules
//! (`crate::config::validate`), not separate types.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use secrecy::SecretString;
use url::Url;

/// Everything a Platform binary must know before it can serve.
///
/// One shape for all three roles; role-specific requirements are validation rules, not separate
/// types, so there is one thing to document and one thing to test.
///
/// `Serialize` exists for exactly one reason — it seeds the built-in defaults provider.
/// The one secret member is `#[serde(skip_serializing)]`, so a default can never carry a secret and
/// a serialized configuration can never leak one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformConfig {
    /// The operator listener. Every role binds one.
    pub admin: AdminConfig,

    /// The public listener. Present for `Edge` only at milestone 1 (rule V1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public: Option<PublicConfig>,

    /// The `PostgreSQL` connection. Optional until the first route that reads persisted data, which
    /// is milestone 5; a binary configured without it starts, serves its probes, and reports no
    /// database check. That is deliberately not "degraded": at milestone 2 and 3 no request path
    /// touches the database, so claiming degradation would make readiness lie in the safe direction,
    /// which is still a lie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseConfig>,

    /// The event bus. Optional: milestones 1 to 5 ran without one, and a developer polling
    /// `/v2/operations` needs no broker. A deployment without it accumulates commands nobody
    /// publishes, which the process warns about at startup rather than discovering later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus: Option<BusConfig>,

    /// What this deployment needs to authenticate people through another service.
    ///
    /// Always present, with every member optional: a deployment without Telegram sign-in and
    /// without provider OAuth configures nothing here, and the routes that need one of them then
    /// refuse rather than half-work.
    #[serde(default)]
    pub identity: IdentityConfig,

    /// The two phases of a graceful stop.
    pub shutdown: ShutdownConfig,

    /// Logging, filtering and span export.
    pub telemetry: TelemetryConfig,
}

/// The operator plane: `/health/live`, `/health/ready`, `/metrics`, `/version`. Never the public API
/// (`AGENTS.md`: "Keep admin and diagnostic endpoints separate from the public user surface").
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    /// `RATATOSKR__ADMIN__BIND`. Default `127.0.0.1:<role default port>`.
    ///
    /// Loopback by default because `SECURITY.md` says "deny by default". The deployment sets
    /// `0.0.0.0:<port>`, because the metrics stack on the target is a container on the Docker bridge
    /// and a host loopback port is not reachable from there; what bounds the exposure is
    /// `IPAddressAllow=` in the unit, not the bind address (ADR-0013). The default stays loopback
    /// because an any-address default silently exposes `/metrics` on a developer's LAN, and one
    /// variable in an environment file is a loud, deliberate override.
    pub bind: SocketAddr,
}

/// The public client surface. `Edge` only at milestone 1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicConfig {
    /// `RATATOSKR__PUBLIC__BIND`. Default `127.0.0.1:8080`.
    pub bind: SocketAddr,

    /// `RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS`. 1..=300, default 15.
    /// `ARCHITECTURE.md` S5.2 layer 2, "transport limits and timeouts".
    ///
    /// The serde default is REQUIRED, not cosmetic: without it, setting only
    /// `RATATOSKR__PUBLIC__BIND` on a role whose defaults omit the `public` table fails extraction
    /// with `MissingField`, and validation rule V1 can never produce its message.
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,

    /// `RATATOSKR__PUBLIC__MAX_BODY_BYTES`. `1024..=104_857_600`, default `1_048_576`.
    /// `ARCHITECTURE.md` S14, "Edge applies request, body, concurrency, and per-actor limits";
    /// `THREAT_MODEL.md`, "Ingress/upload abuse". Same serde-default requirement as above.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
}

/// The `PostgreSQL` connection Platform owns. `identity` and `operations` only; ARCHITECTURE S4.2 and
/// S19 invariant 6 forbid this pool ever reaching a domain service's schema.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// `RATATOSKR__DATABASE__URL`. The whole URL is a secret because a `PostgreSQL` URL carries the
    /// password in its user information, so it can never be `Debug`-printed the way the endpoint
    /// URL of the collector is (rule V10).
    #[serde(default, skip_serializing)]
    pub url: SecretString,

    /// `RATATOSKR__DATABASE__MAX_CONNECTIONS`. 1..=100, default 10.
    ///
    /// A ceiling, not a target. `PostgreSQL`'s own `max_connections` is the real limit and three
    /// roles share it; a pool that can exhaust the server is a self-inflicted outage.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// `RATATOSKR__DATABASE__ACQUIRE_TIMEOUT_SECONDS`. 1..=30, default 5.
    ///
    /// Bounded well below the public request timeout so a saturated pool surfaces as a fast,
    /// truthful failure rather than as a request that times out with no explanation.
    #[serde(default = "default_acquire_timeout_seconds")]
    pub acquire_timeout_seconds: u64,
}

const fn default_max_connections() -> u32 {
    10
}

const fn default_acquire_timeout_seconds() -> u64 {
    5
}

/// The `NATS` server the outbox publishes to and the projection consumes from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusConfig {
    /// `RATATOSKR__BUS__URL`. A `nats://` or `tls://` URL.
    ///
    /// Not a `SecretString`, and validated (rule V13) to carry no user information: a `NATS`
    /// credential belongs in a credentials file the process reads by path, not in a URL that the
    /// effective-configuration log line prints. Same reasoning as rule V10 for the collector.
    pub url: Url,

    /// `RATATOSKR__BUS__NKEY_SEED_PATH`. The file holding this role's `NATS` nkey **seed**.
    ///
    /// A path rather than the value, which is what rule V13 means by "a credentials file read by
    /// path": the seed never appears in the environment, in `Debug`, or in the
    /// effective-configuration log line, and its permissions are the file system's job rather than
    /// a promise about who can read `/proc/<pid>/environ`.
    ///
    /// An nkey rather than a `.creds` file (ADR-0013). A `.creds` file carries its permissions
    /// inside an account JWT, so the answer to "what may `ratatoskr-ingest` publish?" would live in
    /// a signed blob on the host; with nkeys it lives in `deploy/nats/ratatoskr.conf`, in the
    /// repository, where a change to it is a diff somebody reviews.
    ///
    /// Absent means an anonymous connection, which is what `compose.yaml` serves and what no
    /// deployment should.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nkey_seed_path: Option<std::path::PathBuf>,
}

/// What Platform needs in order to believe another service about who somebody is.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    /// `RATATOSKR__IDENTITY__ASSERTION_KEY`. The Ed25519 PUBLIC key of `ratatoskr-telegram`,
    /// base64 with padding, decoding to exactly 32 bytes (rule V14).
    ///
    /// Not a `SecretString`, and that is the point of ADR-0011: Platform holds only the public half,
    /// so it can verify an assertion and cannot issue one. A compromise of the process on the public
    /// internet yields the ability to check signatures, which is worth nothing.
    ///
    /// Absent means the Telegram exchange is not served: the route refuses, and
    /// `GET /v2/capabilities` reports `telegram.mini_app` as unavailable so a client can tell before
    /// it tries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion_key: Option<String>,

    /// `RATATOSKR__IDENTITY__OAUTH_COMPLETION_URL`. Where a browser is sent after a provider
    /// callback has been relayed.
    ///
    /// Configured, never taken from the callback. Every parameter of that request is
    /// attacker-supplied, so a redirect target read out of one is an open redirect — the one
    /// vulnerability an OAuth facade is most likely to ship (ADR-0012).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_completion_url: Option<Url>,
}

/// The two phases of a graceful stop. They are separate knobs because they answer different
/// questions: how long until the load balancer notices, and how long a request may take.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    /// `RATATOSKR__SHUTDOWN__DRAIN_SECONDS`. 0..=60, default 5.
    ///
    /// Seconds to keep serving after SIGTERM while readiness already reports 503, so whatever is
    /// routing to this process stops before the listener closes. On the deployment target that is
    /// `cloudflared`, which retries a closed connection; there is no load balancer to drain from and
    /// no second instance to drain towards (ADR-0010). Zero is legal and means in-flight requests
    /// are the only thing the grace window protects.
    #[serde(default = "default_drain_seconds")]
    pub drain_seconds: u64,

    /// `RATATOSKR__SHUTDOWN__GRACE_SECONDS`. 1..=120, default 25.
    /// Seconds allowed for in-flight requests after the listener stops accepting.
    #[serde(default = "default_grace_seconds")]
    pub grace_seconds: u64,
}

/// Logging and span export.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// `RATATOSKR__TELEMETRY__LOG_FORMAT`. Default `json`.
    #[serde(default)]
    pub log_format: LogFormat,

    /// `RATATOSKR__TELEMETRY__LOG_FILTER`. A `tracing_subscriber::EnvFilter` directive string.
    /// Default `info,tower_http=info,hyper=warn,h2=warn`.
    /// Validated at startup (V5), not at subscriber construction, so a bad filter is a
    /// configuration error on stderr rather than a failure inside telemetry initialisation.
    #[serde(default = "default_log_filter")]
    pub log_filter: String,

    /// `RATATOSKR__TELEMETRY__OTLP__*`. Absent means no span exporter.
    ///
    /// Absence does NOT mean absent trace ids: an `SdkTracerProvider` with zero span processors
    /// still mints a valid, sampled, non-zero W3C trace id, so `trace_id` is real in every log line
    /// and every `ErrorEnvelope` with no collector deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp: Option<OtlpConfig>,
}

/// How a log line is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// One JSON object per line. The default, because production log collectors parse it.
    #[default]
    Json,
    /// Human-readable, for `cargo run`.
    Pretty,
}

/// The OTLP span exporter.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtlpConfig {
    /// `RATATOSKR__TELEMETRY__OTLP__ENDPOINT`, e.g. `https://collector.example:4317`.
    pub endpoint: Url,

    /// `RATATOSKR__TELEMETRY__OTLP__TIMEOUT_SECONDS`. 1..=60, default 5.
    #[serde(default = "default_otlp_timeout_seconds")]
    pub timeout_seconds: u64,

    /// `RATATOSKR__TELEMETRY__OTLP__HEADERS__<NAME>` — collector authentication.
    ///
    /// The ONLY secret in milestone 1. `Debug` renders `[REDACTED]` even nested four levels deep;
    /// there is no `Display`; `skip_serializing` means it cannot be written out; the value is
    /// zeroized on drop; and `rg expose_secret` enumerates every site that has ever touched the
    /// plaintext.
    #[serde(default, skip_serializing)]
    pub headers: BTreeMap<String, SecretString>,
}

/// The default of [`PublicConfig::request_timeout_seconds`]. Written once, here.
pub(super) fn default_request_timeout_seconds() -> u64 {
    15
}

/// The default of [`PublicConfig::max_body_bytes`].
pub(super) fn default_max_body_bytes() -> u64 {
    1_048_576
}

/// The default of [`ShutdownConfig::drain_seconds`].
pub(super) fn default_drain_seconds() -> u64 {
    5
}

/// The default of [`ShutdownConfig::grace_seconds`].
pub(super) fn default_grace_seconds() -> u64 {
    25
}

/// The default of [`TelemetryConfig::log_filter`].
pub(super) fn default_log_filter() -> String {
    "info,tower_http=info,hyper=warn,h2=warn".to_owned()
}

/// The default of [`OtlpConfig::timeout_seconds`].
pub(super) fn default_otlp_timeout_seconds() -> u64 {
    5
}
