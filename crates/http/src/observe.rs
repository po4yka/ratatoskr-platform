//! THE public-router middleware, and the layer stack it sits on top of.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderValue, Method, StatusCode};
use platform_core::config::PublicConfig;
use platform_telemetry::correlation;
use platform_telemetry::metrics::{
    HTTP_SERVER_REQUEST_DURATION_SECONDS, OTHER_METHOD, UNMATCHED_ROUTE,
};
use ratatoskr_error_contracts::TraceId;
use ratatoskr_identifiers::EntityRef;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing::Instrument as _;
use tracing::field::Empty;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::fault;
use crate::lifecycle::HttpState;

/// The response header the correlation is rendered into. There is no `X-Request-Id`: the
/// correlation IS the request id at milestone 1, and two identifiers for one request is a join
/// nobody wants to write.
const CORRELATION_HEADER: &str = "x-correlation-id";

/// The identity of the unit of work one request represents.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Minted once per request, never re-minted, never accepted from a client.
    pub correlation_id: EntityRef,
    /// `None` only when the span context is invalid.
    pub trace_id: Option<TraceId>,
}

/// The public router: `routes` under the milestone-1 layer stack.
///
/// Layer order, outermost first — the argument order below is reversed because `Router::layer`
/// wraps what it is applied to:
///
/// 1. [`observe`], the one middleware;
/// 2. `CatchPanicLayer`, so a panic is caught INSIDE the request span and logged with its
///    correlation;
/// 3. `TimeoutLayer::with_status_code`, which makes 504 a parameter rather than the layer's
///    hardcoded 408 — Edge is a gateway, so a slow request is a slow upstream;
/// 4. `RequestBodyLimitLayer`, which rejects on `content-length` before the inner service runs;
/// 5. the fallback, which is the only route milestone 1 has.
///
/// The middleware sits outside everything, so it has the last word on every byte that leaves the
/// process — including responses that framework code produced, which no handler-level convention
/// can reach.
///
/// `routes` is empty in every binary at milestone 1; it is a parameter because the tests of the
/// invariant this router exists to establish need a handler to fail in.
pub fn public_router(state: Arc<HttpState>, config: &PublicConfig, routes: Router) -> Router {
    let limit = usize::try_from(config.max_body_bytes).unwrap_or(usize::MAX);
    routes
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(config.request_timeout_seconds),
        ))
        .layer(CatchPanicLayer::custom(fault::panic_response))
        .layer(axum::middleware::from_fn_with_state(state, observe))
}

/// The one middleware on the public router. SEVEN numbered responsibilities and no eighth.
///
/// 1. Read [`MatchedPath`] — the route TEMPLATE — or the constant `<unmatched>`, and the method as
///    one of the nine RFC 9110 tokens or `<other>`. The raw URI, the query string, the raw method
///    token and every header stay out of telemetry. This is the redaction mechanism, and the whole
///    bound on label cardinality, not a filter applied afterwards.
/// 2. [`correlation::extract_parent`] — continue an inbound W3C `traceparent`; a malformed one
///    starts a new trace and never fails the request.
/// 3. [`correlation::mint_correlation`] — a fresh `EntityRef`. A client-supplied
///    `x-correlation-id` is ignored.
/// 4. Open `http.server.request`, set its parent, record `trace_id`, and run the inner service
///    inside it.
/// 5. If the response status is `>= 400`, replace the body with [`fault::render`]. At milestone 1
///    no handler authors a body on a failure, so the rule is total and unconditional.
///    ponytail: unconditional replace. When milestone 5 adds a handler that authors its own
///    envelope, that pull request adds a marker extension and this becomes "replace unless already
///    marked".
/// 6. Set `x-correlation-id` on EVERY response, success or failure, so one string finds the log
///    line, the trace and the error body.
/// 7. Record `http_server_request_duration_seconds` and emit the completion event at the level
///    [`PlatformError::log`] chose.
pub(crate) async fn observe(
    State(state): State<Arc<HttpState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let route = request.extensions().get::<MatchedPath>().map_or_else(
        || UNMATCHED_ROUTE.to_owned(),
        |path| path.as_str().to_owned(),
    );
    let method = method_label(request.method());
    let parent = correlation::extract_parent(request.headers());
    let correlation_id = correlation::mint_correlation();

    let span = tracing::info_span!(
        "http.server.request",
        role = state.role.as_str(),
        "http.request.method" = method,
        "http.route" = %route,
        "http.response.status_code" = Empty,
        correlation_id = %correlation_id,
        trace_id = Empty,
        "error.code" = Empty,
    );
    if let Err(error) = span.set_parent(parent) {
        // A trace context that cannot be adopted never fails the request: the span simply starts a
        // new trace, which is exactly what an absent `traceparent` does.
        tracing::debug!(?error, "the inbound trace context could not be adopted");
    }
    let trace_id = correlation::trace_id_of(&span);
    if let Some(id) = trace_id.as_ref() {
        span.record("trace_id", id.as_str());
    }
    let context = RequestContext {
        correlation_id,
        trace_id,
    };

    // Handed to the handler through the request, so a route that needs the correlation — to stamp an
    // operation or a command with it — uses the one this request already has. Minting a second would
    // break ADR-0007's promise that one request carries one correlation for its whole life.
    request.extensions_mut().insert(context.clone());

    let started = Instant::now();
    state.in_flight.fetch_add(1, Ordering::AcqRel);
    let response = next.run(request).instrument(span.clone()).await;
    state.in_flight.fetch_sub(1, Ordering::AcqRel);
    let elapsed = started.elapsed();

    span.in_scope(|| {
        let status = response.status();
        span.record("http.response.status_code", status.as_u16());

        // Literally `>= 400`, not `is_client_error() || is_server_error()`: a status outside
        // both classes must not escape the public listener without an envelope either.
        let mut response = if status.as_u16() >= 400 {
            let error = fault::classify(&response);
            span.record("error.code", error.fault().code.as_str());
            // The completion event, at the level the taxonomy chose, with the full source chain.
            error.log();
            fault::render(&error, &context)
        } else {
            tracing::info!(
                status = status.as_u16(),
                duration_ms = duration_ms(elapsed),
                "request completed"
            );
            response
        };

        // A rendering of an EntityRef is visible ASCII by grammar, so the fallible conversion
        // cannot fail; a response without the header would still be served rather than dropped.
        if let Ok(value) = HeaderValue::from_str(&context.correlation_id.to_string()) {
            response.headers_mut().insert(CORRELATION_HEADER, value);
        }

        metrics::histogram!(
            HTTP_SERVER_REQUEST_DURATION_SECONDS,
            "role" => state.role.as_str(),
            "method" => method,
            "route" => route,
            "status" => status.as_u16().to_string(),
        )
        .record(elapsed.as_secs_f64());

        response
    })
}

/// The `method` label and span field, from the closed RFC 9110 set.
///
/// The same device as [`UNMATCHED_ROUTE`] and for the same reason. A method is an attacker-chosen
/// token off the unauthenticated public listener: hyper accepts a 60 000-byte one, so the raw value
/// is an unauthenticated remote cardinality bomb against the metric registry, against every scrape,
/// and against every log line. Bounding it here bounds both, because the span field and the metric
/// label read the same value.
fn method_label(method: &Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "CONNECT" => "CONNECT",
        _ => OTHER_METHOD,
    }
}

/// A duration as whole milliseconds, saturating rather than wrapping.
pub(crate) fn duration_ms(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}
