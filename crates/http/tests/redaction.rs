//! Redaction and cardinality — tests R-2, R-4 and R-6. The tests that capture the log, R-1, R-2b,
//! R-3, R-5 and T-5, are in `redaction_log.rs`; that file says why.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod redaction_support;

use axum::body::Body;
use http::{Request, StatusCode};
use redaction_support::{MARKER, exposition, label_values, send};
use serde_json::Value;

/// R-2: the 404-scanning cardinality bomb is closed by construction.
#[tokio::test]
async fn a_thousand_unmatched_paths_produce_one_series() {
    for index in 0..1000_u32 {
        let uri = format!("/scan/{MARKER}-{index}");
        let (status, _, _) = send(Request::builder().uri(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    let exposition = exposition();

    assert!(
        !exposition.contains(MARKER),
        "a scanned path reached a label"
    );
    for label in label_values(&exposition, "route") {
        assert!(
            label == "<unmatched>" || label == "/probe/{id}",
            "unexpected route label {label}",
        );
    }
}

/// R-4: internal headers are not trusted from public ingress.
#[tokio::test]
async fn a_client_supplied_correlation_header_is_ignored() {
    let supplied = "correlation:01a01495-1f1d-7973-af61-680840cf4085";

    let (_, headers, body) = send(
        Request::builder()
            .uri("/nope")
            .header("x-correlation-id", supplied)
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    let served = headers.get("x-correlation-id").unwrap().to_str().unwrap();
    assert_ne!(served, supplied);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    assert_ne!(envelope["correlation_id"], supplied);
    assert_eq!(envelope["correlation_id"], served);
}

/// R-6: propagation works and a malformed header is not a denial-of-service surface.
#[tokio::test]
async fn an_inbound_traceparent_is_continued_and_a_malformed_one_does_not_fail_the_request() {
    let inbound = "4bf92f3577b34da6a3ce929d0e0e4736";
    let (status, _, body) = send(
        Request::builder()
            .uri("/nope")
            .header("traceparent", format!("00-{inbound}-00f067aa0ba902b7-01"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(envelope["trace_id"], inbound);

    let (status, _, body) = send(
        Request::builder()
            .uri("/nope")
            .header("traceparent", "not-a-trace-context")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    let fresh = envelope["trace_id"].as_str().unwrap();
    assert_eq!(fresh.len(), 32);
    assert_ne!(fresh, inbound);
}
