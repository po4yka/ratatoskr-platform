//! THE `ErrorEnvelope` construction site, and the `CatchPanicLayer` responder.

use std::any::Any;
use std::fmt;

use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse as _, Response};
use http::StatusCode;
use platform_core::{FailureKind, PlatformError, Subsystem};
use ratatoskr_error_contracts::ErrorEnvelope;

use crate::observe::RequestContext;

/// THE place an [`ErrorEnvelope`] is constructed. There is no other `ErrorEnvelope::new` call in
/// this repository; test F-1 proves it by scanning the source tree.
///
/// `extensions` is left empty. Contracts ADR-0008: a producer never authors a key there, and the
/// testable rule is `extensions.is_empty()` on the envelopes it CONSTRUCTS.
///
/// `field_violations` is left empty: nothing at milestone 1 validates a payload field. The wire
/// shape is already correct because contracts marks it `skip_serializing_if = "Vec::is_empty"`, so
/// milestone 5's addition is provably additive (test F-9).
///
/// The `Internal` arm of `error` is unreachable from here: this function reads
/// [`PlatformError::fault`] and nothing else, so no `subsystem` and no `source` has a path into a
/// response body.
pub(crate) fn render(error: &PlatformError, ctx: &RequestContext) -> Response {
    let fault = error.fault();
    let mut envelope =
        ErrorEnvelope::new(fault.code.clone(), fault.message.clone(), fault.retryable);
    envelope.correlation_id = Some(ctx.correlation_id.clone());
    envelope.trace_id.clone_from(&ctx.trace_id);
    (fault.status, Json(envelope)).into_response()
}

/// The `CatchPanicLayer` responder.
///
/// Extracts the panic payload and carries it in a response extension to the one logging site in
/// [`crate::observe`], which is inside the request span and therefore logs it with the request's
/// correlation. The payload never reaches the response: the body is empty and the middleware
/// replaces it with an `ErrorEnvelope` built from static text.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the tower-http `ResponseForPanic` contract hands the payload over by value"
)]
pub(crate) fn panic_response(payload: Box<dyn Any + Send + 'static>) -> http::Response<Body> {
    let mut response = http::Response::new(Body::empty());
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
        .extensions_mut()
        .insert(CaughtPanic(describe(payload.as_ref())));
    response
}

/// The failure a caught panic represents, carried from [`panic_response`] to the one logging site.
///
/// Cloneable because `http::Extensions` requires it, and an error because it becomes the `source`
/// of a [`PlatformError::Internal`].
#[derive(Debug, Clone)]
pub(crate) struct CaughtPanic(String);

impl fmt::Display for CaughtPanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a request handler panicked: {}", self.0)
    }
}

impl std::error::Error for CaughtPanic {}

/// A status no [`platform_core::FailureKind`] maps to. Reaching this is itself a defect, which is
/// why it is an internal failure and not a silent pass-through.
#[derive(Debug)]
struct UnmappedStatus(StatusCode);

impl fmt::Display for UnmappedStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no failure kind maps to status {}", self.0.as_u16())
    }
}

impl std::error::Error for UnmappedStatus {}

/// A failure a handler chose, carried to the one rendering site.
///
/// A handler names its failure instead of implying it through a status, because a status is
/// ambiguous: 404 is both "no route matched" and "not yours", and both must render differently while
/// looking identical from outside. Carrying the kind rather than a rendered body is what keeps the
/// single-construction-site rule (test F-1) true now that handlers exist.
#[derive(Debug, Clone, Copy)]
pub struct AuthoredFailure(pub FailureKind);

/// A response whose body was validated as a contract `ErrorEnvelope` at an internal boundary.
///
/// `observe` normally replaces every failing body, which is how a public Axum rejection cannot
/// leak a framework response. A reverse proxy is different: it may receive a *validated* envelope
/// from a service that owns the error code. This private marker lets that one boundary preserve the
/// body without creating a second `ErrorEnvelope` construction site.
#[derive(Debug, Clone, Copy)]
struct ValidatedContractError;

/// Refuse a request with a named failure.
///
/// The body is empty on purpose: the middleware renders the envelope, so there is still exactly one
/// place an `ErrorEnvelope` is constructed.
#[must_use]
pub fn reject(kind: FailureKind) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = kind.fault().status;
    response.extensions_mut().insert(AuthoredFailure(kind));
    response
}

/// Preserve a response only after the caller has validated its body as an `ErrorEnvelope`.
///
/// This is intentionally not a general "skip fault rendering" switch: callers can retain a
/// downstream error body only by using the named contract-boundary operation.
#[must_use]
pub fn preserve_contract_error(mut response: Response) -> Response {
    response.extensions_mut().insert(ValidatedContractError);
    response
}

pub(crate) fn is_preserved_contract_error(response: &Response) -> bool {
    response
        .extensions()
        .get::<ValidatedContractError>()
        .is_some()
}

/// The failure a response represents.
///
/// A caught panic first, because it carries diagnostics the status alone cannot; then a failure the
/// handler named; then the static status table for an unauthored response; then an internal failure,
/// because an unmapped status escaping the process is a defect rather than a client-visible fact.
pub(crate) fn classify(response: &Response) -> PlatformError {
    if let Some(panic) = response.extensions().get::<CaughtPanic>() {
        return PlatformError::internal(Subsystem::Http, panic.clone());
    }
    if let Some(authored) = response.extensions().get::<AuthoredFailure>() {
        return PlatformError::Rejected(authored.0);
    }
    PlatformError::from_status(response.status()).unwrap_or_else(|| {
        PlatformError::internal(Subsystem::Http, UnmappedStatus(response.status()))
    })
}

/// The panic payload as text, for the log record only.
fn describe(payload: &(dyn Any + Send + 'static)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_owned();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "the payload is not a string".to_owned()
}
