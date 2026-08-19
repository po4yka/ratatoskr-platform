//! The public fault surface — tests F-1 … F-11.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]
// Separate from the block above because the reason is different, and because clippy has an
// `allow-indexing-slicing-in-tests` configuration key but no `allow-string-slice-in-tests` one, so
// this cannot live in clippy.toml with the others. The text sliced here is a Prometheus exposition
// body and a JSON error body, both ASCII by construction; the lint is denied workspace-wide because
// production code slices header values and idempotency keys, which are not.
#![allow(
    clippy::string_slice,
    reason = "ASCII fixtures parsed by offset in a test binary"
)]

use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::{PlatformConfig, PublicConfig};
use platform_http::{HttpState, public_router};
use platform_telemetry::TelemetryGuard;
use ratatoskr_error_contracts::ErrorEnvelope;
use serde_json::Value;
use tower::ServiceExt as _;

/// A value that must never reach a response body. It is shaped like the worst realistic case: a
/// driver message carrying a host, a port, a user name and the word `password`.
const CANARY: &str = "connection to 10.0.0.4:5432 failed: password authentication failed for user \
                      \"platform_rw\"";

/// The pieces of [`CANARY`] that survive JSON escaping, and therefore the only ones a substring
/// assertion over a response body can honestly look for.
const LEAK_FRAGMENTS: [&str; 5] = ["10.0.0.4", "5432", "password", "platform_rw", "panic"];

/// The response header the correlation is rendered into.
const CORRELATION_HEADER: &str = "x-correlation-id";

/// The subscriber and the W3C propagator, installed once for this process. Without them the span
/// context is invalid and every envelope would omit `trace_id`.
fn install_telemetry() {
    static GUARD: OnceLock<TelemetryGuard> = OnceLock::new();
    let _ = GUARD.get_or_init(|| {
        let config = PlatformConfig::defaults(RuntimeRole::Edge);
        platform_telemetry::init(&config.telemetry, RuntimeRole::Edge)
            .expect("telemetry must install exactly once in this process")
    });
}

/// The public listener of an edge process, with the handlers milestone 1's faults need in order to
/// have a producer.
fn router() -> Router {
    install_telemetry();
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        request_timeout_seconds: 1,
        max_body_bytes: 1024,
    };
    let routes = Router::new()
        .route("/ok", get(|| async { "ok" }))
        .route("/panic", get(boom))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "never"
            }),
        );
    public_router(Arc::new(HttpState::new(RuntimeRole::Edge)), &config, routes)
}

/// A handler that fails the way milestone 1 can actually fail: not at all, until it does.
async fn boom() -> &'static str {
    panic!("{CANARY}")
}

async fn send(request: Request<Body>) -> (StatusCode, http::HeaderMap, String) {
    let response = router().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Every request milestone 1 can fail, and the code each one must carry.
fn faults() -> Vec<(&'static str, Request<Body>, StatusCode, &'static str)> {
    vec![
        (
            "no such route",
            request(Method::GET, "/nope"),
            StatusCode::NOT_FOUND,
            "platform.route.not_found",
        ),
        (
            "wrong method",
            request(Method::POST, "/ok"),
            StatusCode::METHOD_NOT_ALLOWED,
            "platform.route.method_not_allowed",
        ),
        (
            "oversized body",
            oversized(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "platform.request.payload_too_large",
        ),
        (
            "slow handler",
            request(Method::GET, "/slow"),
            StatusCode::GATEWAY_TIMEOUT,
            "platform.request.timeout",
        ),
        (
            "panicking handler",
            request(Method::GET, "/panic"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "platform.internal.error",
        ),
    ]
}

/// A request whose declared `content-length` exceeds `max_body_bytes`.
fn oversized() -> Request<Body> {
    let body = vec![b'x'; 4096];
    Request::builder()
        .method(Method::POST)
        .uri("/ok")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

/// Every permitted `ErrorEnvelope::new` site, with the reason it is permitted. Adding a row here is
/// a deliberate act a reviewer sees; adding a construction site without one fails F-1.
const ALLOWED_ENVELOPE_SITES: [&str; 2] =
    ["crates/http/src/fault.rs", "crates/operations/src/lib.rs"];

/// F-1: every `ErrorEnvelope::new` site in the repository is one this test names and justifies.
///
/// The scan walks the workspace, not this package: `crates/core` also depends on
/// `ratatoskr-error-contracts`, so a construction site there is exactly the regression this test
/// exists to catch, and a package-local scan would not see it.
///
/// The assertion is set equality against an allowlist rather than a count. A count would pass the
/// moment somebody deleted one site and added another, and it would say nothing about why a site is
/// allowed. Two sites exist and they do different things:
///
///   * `crates/http/src/fault.rs` is the ONLY place that authors an envelope as an HTTP RESPONSE
///     BODY. That is the rule milestone 1 established and it is unchanged: no handler writes its
///     own error body.
///   * `crates/operations/src/lib.rs` reconstitutes a diagnostic that is already stored in
///     `operations.operation_errors` into the `errors` field of an `OperationSnapshot`. It is data
///     inside a successful response, not the response to a failure, and routing it through
///     `fault.rs` would mean mapping a stored row onto a `FailureKind` that does not describe it.
#[test]
fn error_envelope_is_constructed_in_exactly_one_place() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let mut scanned = 0_usize;
    let mut sites = Vec::new();
    for tree in ["crates", "services"] {
        visit(&workspace.join(tree), &mut |path, source| {
            scanned += 1;
            if source.contains("ErrorEnvelope::new") {
                sites.push(path.to_owned());
            }
        });
    }

    assert!(
        scanned > 0,
        "the source scan found no files, so it proves nothing"
    );

    // Compare workspace-relative paths. The scan yields absolute paths whose prefix depends on
    // where the checkout lives, so the suffix from the last `crates/` onward is the stable part.
    let mut found: Vec<String> = sites
        .iter()
        .map(|path| {
            let text = path.to_string_lossy().replace('\\', "/");
            text.rfind("crates/")
                .map_or(text.clone(), |index| text[index..].to_owned())
        })
        .collect();
    found.sort();
    found.dedup();

    let mut expected: Vec<String> = ALLOWED_ENVELOPE_SITES
        .iter()
        .map(|site| (*site).to_owned())
        .collect();
    expected.sort();

    assert_eq!(
        found, expected,
        "the set of ErrorEnvelope::new sites changed; every site must be named and justified in \
         ALLOWED_ENVELOPE_SITES above",
    );
}

/// Reads every `.rs` file under a `src/` directory below `root`. Test sources are excluded: they
/// legitimately name what they check, and this file is one of them.
fn visit(root: &Path, seen: &mut impl FnMut(&Path, &str)) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            visit(&path, seen);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).unwrap();
            seen(&path, &source);
        }
    }
}

/// F-2: no variant leaks an internal detail, including through the `source()` chain.
#[tokio::test]
async fn no_variant_leaks_internal_detail() {
    let (status, headers, body) = send(request(Method::GET, "/panic")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    for fragment in LEAK_FRAGMENTS {
        assert!(
            !body.contains(fragment),
            "{fragment} reached the body: {body}"
        );
    }
    let rendered = format!("{headers:?}");
    for fragment in LEAK_FRAGMENTS {
        assert!(
            !rendered.contains(fragment),
            "{fragment} reached a header: {rendered}"
        );
    }
}

/// F-3: every non-2xx public response carries a contract `ErrorEnvelope`.
#[tokio::test]
async fn every_non_2xx_public_response_carries_an_error_envelope() {
    for (name, request, expected, code) in faults() {
        let (status, headers, body) = send(request).await;

        assert_eq!(status, expected, "{name}: {body}");
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json",
            "{name}",
        );
        let envelope: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(envelope["code"], code, "{name}");
        assert!(envelope["message"].is_string(), "{name}");
        assert!(envelope["retryable"].is_boolean(), "{name}");
    }
}

/// F-4: one string finds the log line, the trace and the error body.
#[tokio::test]
async fn every_fault_carries_the_correlation_in_the_body_and_the_header() {
    for (name, request, _, _) in faults() {
        let (_, headers, body) = send(request).await;

        let envelope: Value = serde_json::from_str(&body).unwrap();
        let correlation = envelope["correlation_id"].as_str().unwrap();
        assert!(
            correlation.starts_with("correlation:"),
            "{name}: {correlation}"
        );
        assert_eq!(
            headers.get(CORRELATION_HEADER).unwrap(),
            correlation,
            "{name}",
        );
        assert!(envelope["trace_id"].as_str().unwrap().len() == 32, "{name}");
    }
}

/// F-5: the limit rejects on `content-length`, before any handler runs.
#[tokio::test]
async fn an_oversized_body_returns_413_before_a_handler_runs() {
    let (status, _, body) = send(oversized()).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(envelope["code"], "platform.request.payload_too_large");
    assert_eq!(envelope["retryable"], false);
}

/// F-6: a panic is a 500 with no payload in it, and the process serves the next request.
#[tokio::test]
async fn a_panicking_handler_returns_500_without_the_panic_message_and_the_process_survives() {
    let (status, _, body) = send(request(Method::GET, "/panic")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // Fragments, not the whole CANARY: it embeds `"platform_rw"` with quotes, which serde_json
    // escapes as `\"platform_rw\"`, so a raw `body.contains(CANARY)` can never fail and would pass
    // against a body carrying the entire panic message.
    let envelope: Value = serde_json::from_str(&body).unwrap();
    let message = envelope["message"].as_str().unwrap();
    for fragment in LEAK_FRAGMENTS {
        assert!(!message.contains(fragment), "{fragment} reached: {body}");
    }

    let (status, _, body) = send(request(Method::GET, "/ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

/// F-7: a slow request is a slow upstream, so it is a retryable 504 and not a 408.
#[tokio::test]
async fn a_slow_handler_returns_a_retryable_504_envelope() {
    let (status, _, body) = send(request(Method::GET, "/slow")).await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(envelope["code"], "platform.request.timeout");
    assert_eq!(envelope["retryable"], true);
}

/// F-8: contracts ADR-0008 — a producer never authors a key in `extensions`.
#[tokio::test]
async fn constructed_envelopes_have_empty_extensions() {
    for (name, request, _, _) in faults() {
        let (_, _, body) = send(request).await;

        let envelope: Value = serde_json::from_str(&body).unwrap();
        for member in envelope.as_object().unwrap().keys() {
            assert!(
                ["code", "message", "retryable", "correlation_id", "trace_id"]
                    .contains(&member.as_str()),
                "{name}: unexpected member {member}",
            );
        }
    }
}

/// F-9: locks the milestone-1 shape so milestone 5's field violations are provably additive.
#[tokio::test]
async fn field_violations_are_absent_from_the_wire_when_empty() {
    let (_, _, body) = send(request(Method::GET, "/nope")).await;

    assert!(!body.contains("field_violations"), "{body}");
}

/// F-10: `contracts.toml` names the Rust type as the shape authority, so this IS the schema check.
#[tokio::test]
async fn a_rendered_envelope_round_trips_through_the_contracts_deserializer() {
    let (_, _, body) = send(request(Method::GET, "/nope")).await;

    let envelope: ErrorEnvelope = serde_json::from_str(&body).unwrap();

    assert_eq!(envelope.code.as_str(), "platform.route.not_found");
    assert!(envelope.field_violations.is_empty());
    assert!(envelope.extensions.is_empty());
    assert_eq!(serde_json::to_string(&envelope).unwrap(), body);
}

/// F-11: the correlation is on EVERY response, not only on faults.
#[tokio::test]
async fn every_response_carries_x_correlation_id_including_2xx() {
    let (status, headers, _) = send(request(Method::GET, "/ok")).await;

    assert_eq!(status, StatusCode::OK);
    let correlation = headers.get(CORRELATION_HEADER).unwrap().to_str().unwrap();
    assert!(correlation.starts_with("correlation:"), "{correlation}");

    for (name, request, _, _) in faults() {
        let (_, headers, _) = send(request).await;
        assert!(headers.contains_key(CORRELATION_HEADER), "{name}");
    }
}
