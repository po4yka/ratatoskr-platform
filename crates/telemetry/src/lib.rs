//! Tracing subscriber, OpenTelemetry resource and wire identity, correlation, and metrics.
//!
//! This crate deliberately has no axum: milestone 3 `operations` and milestone 4 `eventing` emit
//! spans and metrics and must not inherit an HTTP server to do it.
//!
//! - [`identity`] — the one wire identity, the build information, and the OpenTelemetry resource.
//! - [`correlation`] — correlation minting and W3C trace-context propagation.
//! - [`metrics`] — the three instrument names, their buckets, and nothing else.
//!
//! # Redaction
//!
//! Three allowlists and no denylist. A secret is a type that cannot be formatted
//! (`secrecy::SecretString`); every span records a closed field set with no header map, no URI, no
//! query string and no body byte; public error bodies are static text. There is exactly one
//! `expose_secret` call in the workspace, on [`init`]'s OTLP path, building the collector
//! metadata map, and the header values it builds are marked sensitive.

pub mod correlation;
pub mod identity;
pub mod metrics;

use std::time::Duration;

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;
use opentelemetry_otlp::{SpanExporter, WithExportConfig as _, WithTonicConfig as _};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use platform_core::RuntimeRole;
use platform_core::config::{LogFormat, OtlpConfig, TelemetryConfig};
use secrecy::ExposeSecret as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Installs the global tracing subscriber, the W3C text-map propagator, and the Prometheus
/// recorder.
///
/// MUST be called from inside a Tokio runtime when `config.otlp` is set: the tonic exporter panics
/// with "there is no reactor running" when constructed outside one.
///
/// Layer order is load-bearing and was established by compilation, not by preference:
///
/// ```text
/// Registry
///   .with(EnvFilter::new(&config.log_filter))
///   .with(tracing_opentelemetry::layer().with_tracer(tracer))   // BEFORE the format layer
///   .with(fmt::layer().json() | .pretty())
/// ```
///
/// `OpenTelemetryLayer<Registry, SdkTracer>` implements only `Layer<Registry>`, so with an explicit
/// `::<Registry>` turbofish it cannot be composed on top of a `Layered<fmt, …>`.
///
/// The `SdkTracerProvider` is built WITH an OTLP batch exporter when `config.otlp` is `Some` and
/// WITHOUT one otherwise. An exporterless provider still mints valid, sampled, non-zero W3C ids, so
/// `trace_id` is real in every log line and every `ErrorEnvelope` on day one with no collector
/// deployed. Sampler `AlwaysOn`; a ratio sampler is one line at the milestone where trace volume
/// costs money.
///
/// The format layer writes to **stdout** with `.with_current_span(true)` and
/// `.with_span_list(false)`, which is what puts the request span's `correlation_id`, `trace_id` and
/// `http.route` onto every event emitted inside a request with no custom formatter.
///
/// # Errors
///
/// [`TelemetryError::Filter`] when `config.log_filter` is not a valid directive string — normally
/// unreachable, because configuration validation rule V5 parses it first.
/// [`TelemetryError::Exporter`] when the OTLP span exporter or the Prometheus recorder cannot be
/// built. [`TelemetryError::AlreadyInstalled`] when this process already installed a subscriber or
/// a global metrics recorder.
pub fn init(config: &TelemetryConfig, role: RuntimeRole) -> Result<TelemetryGuard, TelemetryError> {
    let filter = EnvFilter::try_new(&config.log_filter).map_err(TelemetryError::Filter)?;
    let provider = tracer_provider(config, role)?;
    let metrics_handle = install_recorder()?;

    // The OpenTelemetry layer must be composed BEFORE the format layer; see the item documentation.
    let layers = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer(identity::SERVICE_NAME)));

    let installed = match config.log_format {
        LogFormat::Json => layers
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_writer(std::io::stdout),
            )
            .try_init(),
        LogFormat::Pretty => layers
            .with(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_writer(std::io::stdout),
            )
            .try_init(),
    };
    installed.map_err(|_| TelemetryError::AlreadyInstalled)?;

    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    ::metrics::gauge!(
        metrics::PLATFORM_BUILD_INFO,
        "role" => role.as_str(),
        "version" => identity::VERSION,
        "git_sha" => identity::GIT_SHA,
        "rust_version" => identity::RUST_VERSION,
    )
    .set(1.0);

    Ok(TelemetryGuard {
        provider,
        metrics_handle,
    })
}

/// Owns the tracer provider and the Prometheus handle for the lifetime of the process.
#[derive(Debug)]
pub struct TelemetryGuard {
    /// Kept alive so spans keep resolving; shut down explicitly, never from `Drop`.
    provider: SdkTracerProvider,
    /// The Prometheus text-exposition renderer.
    metrics_handle: PrometheusHandle,
}

impl TelemetryGuard {
    /// The Prometheus text-exposition renderer, for `GET /metrics`.
    #[must_use]
    pub fn metrics_handle(&self) -> PrometheusHandle {
        self.metrics_handle.clone()
    }

    /// Flushes the span exporter and releases the providers. Idempotent.
    ///
    /// Called explicitly at the end of the shutdown sequence and NOT from `Drop`: a `Drop` that
    /// blocks on a network flush during a panic unwind is how a pod hangs.
    pub fn shutdown(self) {
        // An already-shut-down provider reports `AlreadyShutdown` rather than panicking, which is
        // what makes a second shutdown signal (§6.7) safe.
        if let Err(error) = self.provider.shutdown() {
            tracing::warn!(%error, "the span exporter did not shut down cleanly");
        }
    }
}

/// Why telemetry could not be installed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TelemetryError {
    /// `telemetry.log_filter` is not a valid `EnvFilter` directive string.
    #[error("the log filter is not a valid tracing directive string")]
    Filter(#[source] tracing_subscriber::filter::ParseError),

    /// A span or metric exporter could not be constructed.
    ///
    /// The cause is interpolated because this error is reported at the one moment nothing else can
    /// report: `run` writes it to stderr and exits 1, before any subscriber exists. Without it the
    /// operator sees a sentence with no endpoint, no reason and no hint. It is safe to interpolate
    /// because the source chain carries no header value — an invalid OTLP header name or value
    /// fails with the `http` crate's own message, which names neither the header nor its value, and
    /// rule V10 keeps a credential out of the endpoint.
    #[error("a telemetry exporter could not be constructed: {0}")]
    Exporter(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// This process already installed a global subscriber or a global metrics recorder.
    #[error("a global telemetry subscriber is already installed in this process")]
    AlreadyInstalled,
}

/// The tracer provider for `role`: with an OTLP batch exporter when one is configured, without one
/// otherwise. Both mint valid, sampled, non-zero W3C ids.
fn tracer_provider(
    config: &TelemetryConfig,
    role: RuntimeRole,
) -> Result<SdkTracerProvider, TelemetryError> {
    let builder = SdkTracerProvider::builder()
        .with_resource(identity::resource(role))
        .with_sampler(Sampler::AlwaysOn);

    match config.otlp.as_ref() {
        Some(otlp) => Ok(builder.with_batch_exporter(span_exporter(otlp)?).build()),
        None => Ok(builder.build()),
    }
}

/// The OTLP/gRPC span exporter.
///
/// An `https` endpoint needs both halves of TLS: the `tls-ring` crypto provider, which is a Cargo
/// feature and without which `build()` refuses the endpoint outright, and trust anchors, which
/// `ClientTlsConfig::new()` does NOT carry — the exporter's own default would fail every connection
/// with `UnknownIssuer`. `with_enabled_roots` activates every root set the manifest enabled.
fn span_exporter(otlp: &OtlpConfig) -> Result<SpanExporter, TelemetryError> {
    let mut builder = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otlp.endpoint.as_str())
        .with_timeout(Duration::from_secs(otlp.timeout_seconds));

    if otlp.endpoint.scheme() == "https" {
        builder = builder.with_tls_config(ClientTlsConfig::new().with_enabled_roots());
    }

    if !otlp.headers.is_empty() {
        builder = builder.with_metadata(collector_metadata(otlp)?);
    }

    builder
        .build()
        .map_err(|error| TelemetryError::Exporter(Box::new(error)))
}

/// The collector authentication headers.
///
/// **The only site in the workspace that reads a secret in plaintext.** `rg expose_secret`
/// enumerates it. Every value is marked sensitive, so `http` will not let a proxy log it.
fn collector_metadata(otlp: &OtlpConfig) -> Result<MetadataMap, TelemetryError> {
    let mut headers = http::HeaderMap::with_capacity(otlp.headers.len());
    for (name, secret) in &otlp.headers {
        let name = http::HeaderName::try_from(name.as_str())
            .map_err(|error| TelemetryError::Exporter(Box::new(error)))?;
        let mut value = http::HeaderValue::from_str(secret.expose_secret())
            .map_err(|error| TelemetryError::Exporter(Box::new(error)))?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(MetadataMap::from_headers(headers))
}

/// Installs the global Prometheus recorder with the shared latency buckets.
///
/// `default-features = false` drops the HTTP listener and the push gateway: `/metrics` is one axum
/// route on the admin router calling `handle.render()`, so there is no second HTTP server.
fn install_recorder() -> Result<PrometheusHandle, TelemetryError> {
    PrometheusBuilder::new()
        .set_buckets(&metrics::DURATION_BUCKETS)
        .and_then(PrometheusBuilder::install_recorder)
        .map_err(|error| match error {
            BuildError::FailedToSetGlobalRecorder(_) => TelemetryError::AlreadyInstalled,
            other => TelemetryError::Exporter(Box::new(other)),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value that must never reach a log line, a `Debug` rendering or an error message.
    const CANARY: &str = "Bearer canary-not-a-real-credential";

    fn otlp_config() -> OtlpConfig {
        serde_json::from_value(serde_json::json!({
            "endpoint": "https://collector.example:4317",
            "timeout_seconds": 5,
            "headers": { "authorization": CANARY },
        }))
        .expect("the OTLP fixture must deserialize")
    }

    /// The one `expose_secret` call reaches the wire, and nowhere else.
    #[test]
    fn collector_metadata_carries_the_header_and_never_renders_it() {
        let config = otlp_config();

        let metadata = collector_metadata(&config).expect("the metadata map must build");
        assert_eq!(
            metadata
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(CANARY),
        );

        assert!(
            !format!("{config:?}").contains("canary"),
            "a SecretString must render as [REDACTED] however deeply it is nested",
        );
    }

    /// An unusable header name or value fails as a build error that names neither the header nor
    /// its value. `TelemetryError::Exporter` interpolates its source, so this is what makes that
    /// interpolation safe.
    #[test]
    fn a_rejected_header_never_appears_in_the_error() {
        for (name, value) in [
            ("not a header name", CANARY.to_owned()),
            // A control character makes the VALUE unrepresentable, which is the other arm.
            ("authorization", format!("{CANARY}\n")),
        ] {
            let mut config = otlp_config();
            config.headers.insert(name.to_owned(), value.into());

            let error = collector_metadata(&config).expect_err("an invalid header must fail");
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("canary"), "got {rendered}");
        }
    }

    /// The `https` scheme rule V7 accepts and `.env.example` advertises is a scheme the exporter
    /// can actually be built for. Without the `tls-*` features this fails and every process
    /// configured for a TLS-terminated collector exits 1 at startup.
    #[tokio::test]
    async fn the_span_exporter_builds_for_an_https_endpoint() {
        // The endpoint is connected to lazily, so this builds without a collector.
        span_exporter(&otlp_config()).expect("an https endpoint must be exportable");
    }
}
