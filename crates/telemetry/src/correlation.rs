//! Correlation minting and W3C trace-context propagation.
//!
//! Two independent axes. The correlation is minted server-side, always, and is a contracts
//! [`EntityRef`] at every layer. The trace context is read from the caller when it offers a valid
//! `traceparent` and started fresh otherwise.

use opentelemetry::Context;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::TraceContextExt as _;
use ratatoskr_error_contracts::TraceId;
use ratatoskr_identifiers::{CorrelationId, EntityRef};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// Mints the correlation identity for a unit of user-visible work that has no owning operation.
///
/// Returns `correlation:<uuid7>` as a contracts [`EntityRef`] — never a `String`, at any layer, at
/// any milestone (ADR-0007). Contracts ships [`CorrelationId`] for exactly this case: "A
/// correlation identity minted by a producer for work not bound to an operation."
#[must_use]
pub fn mint_correlation() -> EntityRef {
    CorrelationId::new_v7().as_entity_ref()
}

/// Extracts a W3C `traceparent` from request headers into an OpenTelemetry context.
///
/// An absent or malformed header yields an empty context, which starts a NEW trace; it never fails
/// the request. A client-chosen trace id is accepted because contracts documents [`TraceId`] as
/// "for log correlation only; never a business key and never an authorization input", so it is not
/// an authorization surface and a hostile value costs nothing.
///
/// A client-supplied `x-correlation-id` is NEVER accepted, under any configuration. Correlation is
/// always minted server-side (`AGENTS.md`: internal headers are not trusted from public ingress),
/// which is why this function reads the W3C fields and nothing else.
#[must_use]
pub fn extract_parent(headers: &http::HeaderMap) -> Context {
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderMapExtractor(headers))
    })
}

/// The W3C trace id of `span` as a contracts [`TraceId`].
///
/// `None` when the span context is invalid, so an all-zero id is omitted rather than emitted as
/// thirty-two zeros.
#[must_use]
pub fn trace_id_of(span: &tracing::Span) -> Option<TraceId> {
    let context = span.context();
    let span_context = context.span().span_context().clone();
    if !span_context.is_valid() {
        return None;
    }
    TraceId::parse(&span_context.trace_id().to_string()).ok()
}

/// Reads W3C text-map fields out of an `http::HeaderMap`.
///
/// `opentelemetry-http` exists for this, but it is a whole dependency for one trait with two
/// methods, and it pins its own `http` major version.
struct HeaderMapExtractor<'a>(&'a http::HeaderMap);

impl Extractor for HeaderMapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}
