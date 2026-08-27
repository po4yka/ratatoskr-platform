//! The closed public failure taxonomy — tests E-1 … E-6.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeSet;
use std::io;

use http::StatusCode;
use platform_core::error::{FailureKind, PlatformError, PublicFault, Subsystem};
use ratatoskr_error_contracts::ErrorCode;
use ratatoskr_identifiers::SafeMessage;

/// The projection of an internal failure. It has no `FailureKind`, which is the point: nothing can
/// route caller-influenced text into it.
fn internal_fault() -> &'static PublicFault {
    let error = PlatformError::internal(
        Subsystem::Http,
        io::Error::other("a diagnostic no client may read"),
    );
    error.fault()
}

/// Every fault the process can render, including the unclassified internal one.
fn every_fault() -> Vec<&'static PublicFault> {
    let mut faults: Vec<&'static PublicFault> = FailureKind::ALL
        .into_iter()
        .map(FailureKind::fault)
        .collect();
    faults.push(internal_fault());
    faults
}

/// E-1. The `#[allow(expect_used)]` on the static tables can never fire in production, and a
/// consumer attributes the failure from the head segment without a lookup table.
#[test]
fn every_failure_kind_has_a_parsable_error_code_in_the_platform_context() {
    for fault in every_fault() {
        let reparsed = ErrorCode::parse(fault.code.as_str())
            .unwrap_or_else(|error| panic!("{} must parse: {error}", fault.code.as_str()));
        assert_eq!(&reparsed, &fault.code);
        assert!(
            matches!(fault.code.bounded_context(), "platform" | "edge"),
            "{} must be owned by Platform or its public Edge boundary",
            fault.code.as_str()
        );
        let segments = fault.code.as_str().split('.').count();
        assert!(
            segments == 3 || (fault.code.bounded_context() == "edge" && segments == 2),
            "{} must be a platform three-segment or Edge two-segment code",
            fault.code.as_str()
        );
    }
}

/// E-2. The same guarantee for the message: `INTERFACES.md` requires stable error envelopes, and a
/// message that does not parse would panic the table at first use.
#[test]
fn every_failure_kind_message_is_a_parsable_safe_message() {
    for fault in every_fault() {
        let reparsed = SafeMessage::parse(fault.message.as_str())
            .unwrap_or_else(|error| panic!("{} must parse: {error}", fault.message.as_str()));
        assert_eq!(&reparsed, &fault.message);
    }
}

/// E-3. `AGENTS.md` principle 5: a client branching on the code branches unambiguously.
#[test]
fn failure_kind_codes_are_unique() {
    let codes: BTreeSet<&str> = every_fault().iter().map(|f| f.code.as_str()).collect();
    assert_eq!(codes.len(), FailureKind::ALL.len() + 1);
}

/// E-4. The status and retryability table is pinned, so changing it must be a deliberate edit
/// (`ARCHITECTURE.md` S5.5: retryability is explicit, never inferred).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "a data table with one row per failure kind and a comment on the rows whose status or \
              retryability is a decision rather than an obvious mapping; splitting it would put half \
              the contract in one function and half in another"
)]
fn the_status_and_retryability_table_is_pinned() {
    let expected = [
        (
            FailureKind::RouteNotFound,
            StatusCode::NOT_FOUND,
            "platform.route.not_found",
            false,
            "No such resource.",
        ),
        (
            FailureKind::MethodNotAllowed,
            StatusCode::METHOD_NOT_ALLOWED,
            "platform.route.method_not_allowed",
            false,
            "That method is not allowed for this resource.",
        ),
        (
            FailureKind::PayloadTooLarge,
            StatusCode::PAYLOAD_TOO_LARGE,
            "platform.request.payload_too_large",
            false,
            "The request body exceeds the permitted size.",
        ),
        (
            // 504 and not 408: RFC 9110 15.5.9 makes 408 a statement that the CLIENT was slow.
            FailureKind::RequestTimeout,
            StatusCode::GATEWAY_TIMEOUT,
            "platform.request.timeout",
            true,
            "The request took too long and was abandoned.",
        ),
        (
            FailureKind::Unauthenticated,
            StatusCode::UNAUTHORIZED,
            "platform.auth.unauthenticated",
            false,
            "Authentication is required.",
        ),
        (
            FailureKind::Forbidden,
            StatusCode::FORBIDDEN,
            "platform.auth.forbidden",
            false,
            "You are not allowed to perform this action.",
        ),
        (
            // The same status and the same words as RouteNotFound, and a different code. From
            // outside they are indistinguishable, which is what S15 requires; from inside they are
            // different facts, which is why the code differs.
            FailureKind::NotFound,
            StatusCode::NOT_FOUND,
            "platform.resource.not_found",
            false,
            "No such resource.",
        ),
        (
            FailureKind::InvalidRequest,
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            false,
            "The request body is not valid for this endpoint.",
        ),
        (
            FailureKind::MissingIdempotencyKey,
            StatusCode::BAD_REQUEST,
            "platform.request.idempotency_key_required",
            false,
            "This endpoint requires an Idempotency-Key header.",
        ),
        (
            // Not retryable: repeating the identical request conflicts again. The client waits for
            // the first attempt or sends a new key.
            FailureKind::IdempotencyConflict,
            StatusCode::CONFLICT,
            "platform.request.idempotency_conflict",
            false,
            "That idempotency key is in use for a different request.",
        ),
        (
            // Retryable, and the wait is the point: the allowance refills on its own, so the same
            // request succeeds later with no operator action.
            FailureKind::RateLimited,
            StatusCode::TOO_MANY_REQUESTS,
            "platform.limit.rate_exceeded",
            true,
            "Too many requests. Slow down and try again.",
        ),
        (
            // Retryable for a different reason: nothing about THIS caller is wrong, the process is
            // simply at its concurrency bound, and shedding is how it stays inside one.
            FailureKind::Overloaded,
            StatusCode::SERVICE_UNAVAILABLE,
            "platform.limit.overloaded",
            true,
            "The service is busy. Try again shortly.",
        ),
        (
            FailureKind::UpstreamUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            "edge.upstream_unavailable",
            true,
            "The requested service is temporarily unavailable.",
        ),
        (
            FailureKind::UpstreamInvalidResponse,
            StatusCode::BAD_GATEWAY,
            "edge.upstream_invalid_response",
            true,
            "The requested service returned an invalid response.",
        ),
        (
            FailureKind::UpstreamTimeout,
            StatusCode::GATEWAY_TIMEOUT,
            "edge.upstream_timeout",
            true,
            "The requested service took too long to respond.",
        ),
    ];
    assert_eq!(
        expected.len(),
        FailureKind::ALL.len(),
        "a new kind needs a row"
    );

    for (kind, status, code, retryable, message) in expected {
        let fault = kind.fault();
        assert_eq!(fault.status, status, "{code}");
        assert_eq!(fault.code.as_str(), code);
        assert_eq!(fault.retryable, retryable, "{code}");
        assert_eq!(fault.message.as_str(), message);
        assert_eq!(kind.to_string(), code, "Display writes the code");
        assert_eq!(PlatformError::Rejected(kind).status(), status);
    }

    let internal = internal_fault();
    assert_eq!(internal.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(internal.code.as_str(), "platform.internal.error");
    assert_eq!(
        internal.message.as_str(),
        "The request could not be completed."
    );
    assert!(
        !internal.retryable,
        "an unclassified internal failure carries no promise of transience"
    );
}

/// E-5. Every status the process can produce maps back to a failure, so an unmapped status cannot
/// escape unenveloped.
#[test]
fn from_status_is_total_over_every_status_the_process_can_produce() {
    for kind in FailureKind::UNAUTHORED {
        let status = kind.fault().status;
        let recovered = PlatformError::from_status(status)
            .unwrap_or_else(|| panic!("{status} must map back to a failure"));
        assert!(matches!(recovered, PlatformError::Rejected(found) if found == kind));
        assert_eq!(recovered.status(), status);
    }

    // An AUTHORED failure is deliberately not recoverable from its status. 404 is both "no route
    // matched" and "not yours"; 400 is both an invalid body and a missing idempotency key. A handler
    // names its failure in a response extension instead, which is what keeps both readings possible
    // while they look identical from outside.
    for kind in FailureKind::ALL {
        if FailureKind::UNAUTHORED.contains(&kind) {
            continue;
        }
        let recovered = PlatformError::from_status(kind.fault().status);
        assert!(
            recovered.is_none()
                || !matches!(recovered, Some(PlatformError::Rejected(found)) if found == kind),
            "{kind:?} must not be recoverable from its status alone"
        );
    }

    // 500 is deliberately unmapped: a caught panic is constructed as an internal failure with its
    // payload as the source, and the caller renders any other unmapped status the same way.
    assert!(PlatformError::from_status(StatusCode::INTERNAL_SERVER_ERROR).is_none());
    assert!(PlatformError::from_status(StatusCode::OK).is_none());
    assert!(PlatformError::from_status(StatusCode::IM_A_TEAPOT).is_none());
}

/// E-6. A contract-dependency test: a contracts bump that relaxed `SafeMessage` fails here, because
/// the newline ban is what stops a stack trace or a forged log line reaching a wire message.
#[test]
fn a_multiline_message_cannot_become_a_safe_message() {
    assert!(SafeMessage::parse("first line\nsecond line").is_err());
    assert!(SafeMessage::parse("carriage\rreturn").is_err());
    assert!(SafeMessage::parse("null\u{0}byte").is_err());
    assert!(SafeMessage::parse("a single safe line.").is_ok());
}

/// The internal arm keeps its diagnostics reachable for the log and unreachable for the client:
/// the source chain survives, and the public projection does not mention it.
#[test]
fn internal_diagnostics_stay_in_the_source_chain() {
    let error = PlatformError::internal(Subsystem::Config, io::Error::other("inner detail"));

    assert!(std::error::Error::source(&error).is_some());
    assert_eq!(error.to_string(), "internal failure in config");
    assert!(!error.fault().message.as_str().contains("inner detail"));
}
