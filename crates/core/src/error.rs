//! The platform error taxonomy and its closed public projection.
//!
//! The two-arm split of [`PlatformError`] is the security property: the client-visible arm has a
//! unit payload, so no caller-influenced or dependency-authored text has anywhere to sit, and the
//! diagnostics arm carries data that the envelope renderer cannot read.

use std::sync::LazyLock;

use http::StatusCode;
use ratatoskr_error_contracts::ErrorCode;
use ratatoskr_identifiers::SafeMessage;

/// Everything that can fail inside a Platform process and reach an HTTP boundary.
///
/// The two-arm split IS the security property. [`PlatformError::Rejected`] has a unit payload, so
/// there is nowhere for caller-influenced or dependency-authored text to sit in the client-visible
/// arm; a caller who wants to smuggle a provider message into a 4xx has to change this enum, which
/// is a reviewed diff. [`PlatformError::Internal`] carries diagnostics that the envelope renderer
/// cannot read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlatformError {
    /// A failure the caller may see and act on. The public code, status, retryability and message
    /// all come from the kind's static table.
    #[error("{0}")]
    Rejected(FailureKind),

    /// A failure inside Platform. `source` is logged exactly once, at the boundary, and never
    /// serialized.
    #[error("internal failure in {subsystem}")]
    Internal {
        /// Which part of the process failed. A telemetry attribute, never a client-visible fact.
        subsystem: Subsystem,
        /// The diagnostics. Logged once at the boundary; never rendered into a response.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl PlatformError {
    /// Constructs an internal failure from any error.
    pub fn internal(
        subsystem: Subsystem,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Internal {
            subsystem,
            source: Box::new(source),
        }
    }

    /// The public projection. Exhaustive; a new variant does not compile until it has one.
    #[must_use]
    pub fn fault(&self) -> &'static PublicFault {
        match self {
            Self::Rejected(kind) => kind.fault(),
            Self::Internal { .. } => &INTERNAL,
        }
    }

    /// The HTTP status this failure renders as.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.fault().status
    }

    /// The failure that a response NO HANDLER AUTHORED represents — axum's own 404 and 405, a
    /// `tower-http` 413 or 504, a caught panic's 500.
    ///
    /// Searches [`FailureKind::UNAUTHORED`], not every kind. A status alone cannot identify an
    /// authored failure: 404 is both "no route matched" and "not yours", and 400 is both an invalid
    /// body and a missing idempotency key. A handler therefore names its failure rather than
    /// implying it through a status, and this function answers only the question it can answer —
    /// what an UNAUTHORED response of this status means.
    ///
    /// `None` for a status Platform does not produce that way; the caller renders that as an
    /// internal failure, because an unmapped status escaping the process is itself a defect.
    #[must_use]
    pub fn from_status(status: StatusCode) -> Option<Self> {
        FailureKind::UNAUTHORED
            .into_iter()
            .find(|kind| kind.fault().status == status)
            .map(Self::Rejected)
    }

    /// Writes the diagnostics exactly once, at the boundary, and nowhere else.
    ///
    /// ERROR with the full `source()` chain for [`PlatformError::Internal`]; WARN for a 5xx
    /// [`PlatformError::Rejected`]; INFO otherwise.
    pub fn log(&self) {
        let fault = self.fault();
        let code = fault.code.as_str();
        let status = fault.status.as_u16();
        match self {
            Self::Internal { subsystem, source } => {
                tracing::error!(
                    subsystem = ?subsystem,
                    code,
                    status,
                    chain = %source_chain(source.as_ref()),
                    "internal failure"
                );
            }
            Self::Rejected(kind) if kind.fault().status.is_server_error() => {
                tracing::warn!(kind = ?kind, code, status, "request rejected");
            }
            Self::Rejected(kind) => {
                tracing::info!(kind = ?kind, code, status, "request rejected");
            }
        }
    }
}

/// The whole `source()` chain of an error, innermost cause last.
fn source_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut chain = error.to_string();
    let mut current = error.source();
    while let Some(cause) = current {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        current = cause.source();
    }
    chain
}

/// Which part of the process failed. Bounded-cardinality telemetry only: never on a wire, never in
/// a response body. Milestone 4 adds `Bus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Subsystem {
    /// Reading or validating the typed configuration.
    Config,
    /// The subscriber, the exporter or an instrument.
    Telemetry,
    /// The HTTP harness: a listener, a middleware, or a handler.
    Http,
    /// The database pool, a migration, or a query.
    Persistence,
}

impl core::fmt::Display for Subsystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Config => "config",
            Self::Telemetry => "telemetry",
            Self::Http => "http",
            Self::Persistence => "persistence",
        })
    }
}

/// The closed public failure taxonomy.
///
/// Each variant fixes a code, a status, a retry class and a message. Nothing here is derived from
/// data, and there is deliberately NO `Internal` variant: an internal failure is unreachable through
/// [`PlatformError::Rejected`], so it is structurally impossible to attach a caller-influenced
/// message or a non-500 status to one.
///
/// Every variant listed has a real producer at milestone 1. `InvalidRequest` (400),
/// `UnsupportedMediaType` (415) and `ServiceUnavailable` (503) are NOT declared, because nothing at
/// milestone 1 validates a payload, negotiates a media type, or depends on a service. They arrive
/// with milestone 5's first route and milestone 2's first dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureKind {
    /// No route matched. Produced by the public router's fallback.
    RouteNotFound,
    /// The route exists, the method does not. Produced by axum's method router.
    MethodNotAllowed,
    /// Rejected by `RequestBodyLimitLayer` before any handler ran.
    PayloadTooLarge,
    /// Rejected by `TimeoutLayer`.
    RequestTimeout,
    /// No credential, or one that does not authenticate. Deliberately one variant and not four: a
    /// missing, malformed, revoked and expired credential must be indistinguishable to a caller, or
    /// the difference is an oracle (`ARCHITECTURE.md` S15).
    Unauthenticated,
    /// The resource does not exist, or exists and is not this principal's. Also deliberately one
    /// variant: S15 requires authorization before existence is disclosed, so "not yours" and "not
    /// there" must read the same from outside.
    NotFound,
    /// The request body is not what the route accepts.
    InvalidRequest,
    /// A required header is absent. `INTERFACES.md` requires `Idempotency-Key` on a replayable
    /// mutation, so its absence is a client error rather than a silently unprotected write.
    MissingIdempotencyKey,
    /// The same idempotency key was reused with a different payload, or its first attempt is still
    /// in flight. `ARCHITECTURE.md` S8.1 rejects the first outright; the second would start the
    /// duplicate work the key exists to prevent.
    IdempotencyConflict,
}

impl FailureKind {
    /// Every kind, in status order. The array length is the documented count, so adding a variant
    /// without updating it does not compile.
    pub const ALL: [Self; 9] = [
        Self::RouteNotFound,
        Self::MethodNotAllowed,
        Self::PayloadTooLarge,
        Self::RequestTimeout,
        Self::Unauthenticated,
        Self::NotFound,
        Self::InvalidRequest,
        Self::MissingIdempotencyKey,
        Self::IdempotencyConflict,
    ];

    /// The kinds a response can arrive as with no handler involved: axum's own routing and
    /// EXTRACTOR failures, and the transport limits `tower-http` enforces. Each has a distinct
    /// status, which is what lets [`PlatformError::from_status`] be a function rather than a guess.
    ///
    /// `InvalidRequest` is here because an extractor that cannot read what it was given rejects the
    /// request itself, before any handler runs. Without an entry, that 400 matched nothing, fell
    /// through to the unmapped-status branch, and reached the client as a **500** — a client error
    /// reported as ours, on every route that takes a typed body or query string. It was found by a
    /// test asserting a public route's answer to an empty request, which is the only kind of route
    /// where nothing else fails first.
    pub const UNAUTHORED: [Self; 5] = [
        Self::RouteNotFound,
        Self::MethodNotAllowed,
        Self::PayloadTooLarge,
        Self::RequestTimeout,
        Self::InvalidRequest,
    ];

    /// The only thing a client ever learns about this failure.
    #[must_use]
    pub fn fault(self) -> &'static PublicFault {
        match self {
            Self::RouteNotFound => &ROUTE_NOT_FOUND,
            Self::MethodNotAllowed => &METHOD_NOT_ALLOWED,
            Self::PayloadTooLarge => &PAYLOAD_TOO_LARGE,
            Self::RequestTimeout => &REQUEST_TIMEOUT,
            Self::Unauthenticated => &UNAUTHENTICATED,
            Self::NotFound => &NOT_FOUND,
            Self::InvalidRequest => &INVALID_REQUEST,
            Self::MissingIdempotencyKey => &MISSING_IDEMPOTENCY_KEY,
            Self::IdempotencyConflict => &IDEMPOTENCY_CONFLICT,
        }
    }
}

impl core::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.fault().code.as_str())
    }
}

/// The only thing a client ever learns about a failure.
///
/// Every member is owned static data produced once by a [`LazyLock`]. There is no `String`, no
/// `format!`, and no borrow of the error anywhere in this type, so no runtime value has a path into
/// a public code, message or status.
#[derive(Debug)]
pub struct PublicFault {
    /// The HTTP status the failure renders as.
    pub status: StatusCode,
    /// The stable, machine-actionable code — the only member a consumer may branch on.
    pub code: ErrorCode,
    /// The human-readable explanation. Never machine-parsed, never stable across releases.
    pub message: SafeMessage,
    /// Whether repeating the identical request may succeed later without operator action.
    /// Explicit, never inferred from the code by a consumer (`ARCHITECTURE.md` S5.5).
    pub retryable: bool,
}

/// Builds one table entry from compile-time contract constants.
#[allow(
    clippy::expect_used,
    reason = "FailureKind's code and message strings are compile-time contract constants, proved \
              parseable by test E-1; a build whose table is malformed is broken before it serves a \
              request"
)]
fn entry(status: StatusCode, code: &str, message: &str, retryable: bool) -> PublicFault {
    PublicFault {
        status,
        code: ErrorCode::parse(code).expect("a fault code must satisfy the contract grammar"),
        message: SafeMessage::parse(message).expect("a fault message must be a safe message"),
        retryable,
    }
}

/// `platform.auth.unauthenticated` — 401.
static UNAUTHENTICATED: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::UNAUTHORIZED,
        "platform.auth.unauthenticated",
        "Authentication is required.",
        false,
    )
});

/// `platform.resource.not_found` — 404. Distinct from `platform.route.not_found`, which means no
/// route matched at all; this one means the route matched and the resource is not available to you.
static NOT_FOUND: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::NOT_FOUND,
        "platform.resource.not_found",
        "No such resource.",
        false,
    )
});

/// `platform.request.invalid` — 400.
static INVALID_REQUEST: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::BAD_REQUEST,
        "platform.request.invalid",
        "The request body is not valid for this endpoint.",
        false,
    )
});

/// `platform.request.idempotency_key_required` — 400.
static MISSING_IDEMPOTENCY_KEY: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::BAD_REQUEST,
        "platform.request.idempotency_key_required",
        "This endpoint requires an Idempotency-Key header.",
        false,
    )
});

/// `platform.request.idempotency_conflict` — 409.
///
/// Retryable is false on purpose: repeating the identical request will conflict again. The client
/// must either wait for the first attempt or send a new key.
static IDEMPOTENCY_CONFLICT: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::CONFLICT,
        "platform.request.idempotency_conflict",
        "That idempotency key is in use for a different request.",
        false,
    )
});

/// `platform.route.not_found` — 404.
static ROUTE_NOT_FOUND: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::NOT_FOUND,
        "platform.route.not_found",
        "No such resource.",
        false,
    )
});

/// `platform.route.method_not_allowed` — 405.
static METHOD_NOT_ALLOWED: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::METHOD_NOT_ALLOWED,
        "platform.route.method_not_allowed",
        "That method is not allowed for this resource.",
        false,
    )
});

/// `platform.request.payload_too_large` — 413.
static PAYLOAD_TOO_LARGE: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::PAYLOAD_TOO_LARGE,
        "platform.request.payload_too_large",
        "The request body exceeds the permitted size.",
        false,
    )
});

/// `platform.request.timeout` — 504. A gateway timeout, not a 408: RFC 9110 15.5.9 makes 408 a
/// statement that the CLIENT was slow, and Edge is a gateway in front of domain services.
static REQUEST_TIMEOUT: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::GATEWAY_TIMEOUT,
        "platform.request.timeout",
        "The request took too long and was abandoned.",
        true,
    )
});

/// `platform.internal.error` — 500, and never retryable: an unclassified internal failure carries
/// no promise of transience, and marking it retryable invites a retry storm against a service that
/// is already broken.
static INTERNAL: LazyLock<PublicFault> = LazyLock::new(|| {
    entry(
        StatusCode::INTERNAL_SERVER_ERROR,
        "platform.internal.error",
        "The request could not be completed.",
        false,
    )
});
