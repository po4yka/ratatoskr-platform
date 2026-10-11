//! XR-021 R2-02: a progress report is applied only when its tenant is the operation's owner and its
//! producer is allowed for the operation's kind.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_eventing::inbox::Outcome;
use platform_eventing::{Incoming, Subject, deliver};
use platform_operations::ProgressProjection;
use platform_operations::authority::{AUTHORITY, Producers, producers_for};
use platform_persistence::test_support::TestDatabase;
use ratatoskr_identifiers::{Extensions, OperationId};
use ratatoskr_operation_contracts::{OperationReported, OperationStatus};
use uuid::Uuid;

const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";
const OWNER: Uuid = Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0b01);
const OTHER: Uuid = Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0b02);

const PLATFORM_SUBJECT: &str = "evt.platform.operation.reported.v1";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).expect("a fixed timestamp")
}

fn report(operation_id: Uuid) -> serde_json::Value {
    serde_json::to_value(OperationReported {
        operation_id: OperationId(operation_id),
        status: OperationStatus::Running,
        stage: None,
        progress_percent: None,
        results: Vec::new(),
        error: None,
        warnings: Vec::new(),
        extensions: Extensions::default(),
    })
    .expect("the report serializes")
}

/// A report in the envelope a producer publishes. `tenant` is the envelope `tenant_id`, if any.
fn message(producer: &str, tenant: Option<Uuid>, operation_id: Uuid) -> Incoming {
    let mut envelope = serde_json::Map::new();
    envelope.insert("event_id".to_owned(), serde_json::json!(Uuid::now_v7()));
    envelope.insert("producer".to_owned(), serde_json::json!(producer));
    if let Some(tenant) = tenant {
        envelope.insert(
            "tenant_id".to_owned(),
            serde_json::json!(format!("user:{tenant}")),
        );
    }
    envelope.insert("payload".to_owned(), report(operation_id));
    Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::parse(PLATFORM_SUBJECT).expect("the report subject"),
        producer: producer.to_owned(),
        payload: serde_json::Value::Object(envelope),
    }
}

async fn accepted(pool: &sqlx::PgPool, kind: &str) -> Uuid {
    platform_operations::accept(pool, OWNER, kind, CORRELATION, None, now())
        .await
        .expect("accepting")
        .operation_id
}

async fn status(pool: &sqlx::PgPool, operation_id: Uuid) -> OperationStatus {
    platform_operations::find(pool, operation_id)
        .await
        .expect("reading")
        .expect("the operation")
        .status
}

/// Deliver `incoming` and assert the outcome, and, for a rejection, that the status did not move.
async fn expect(pool: &sqlx::PgPool, operation_id: Uuid, incoming: &Incoming, outcome: Outcome) {
    assert_eq!(
        deliver(pool, &ProgressProjection, incoming, now())
            .await
            .expect("delivering"),
        Some(outcome),
        "{} reporting for {operation_id}",
        incoming.producer
    );
    let expected = if outcome == Outcome::Applied {
        OperationStatus::Running
    } else {
        OperationStatus::Accepted
    };
    assert_eq!(status(pool, operation_id).await, expected);
}

#[tokio::test]
async fn a_report_for_another_tenant_is_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted(pool, "content.capture.submit").await;

    let forged = message("ratatoskr-extractor", Some(OTHER), operation);
    expect(pool, operation, &forged, Outcome::Rejected).await;

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_report_without_a_tenant_is_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted(pool, "content.capture.submit").await;

    expect(
        pool,
        operation,
        &message("ratatoskr-extractor", None, operation),
        Outcome::Rejected,
    )
    .await;
    let mut bare = message("ratatoskr-extractor", Some(OWNER), operation);
    bare.payload = report(operation);
    expect(pool, operation, &bare, Outcome::Rejected).await;

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_capture_report_from_a_producer_outside_the_list_is_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted(pool, "content.capture.submit").await;

    for producer in [
        "ratatoskr-chatgpt",
        "ratatoskr-claude",
        "ratatoskr-channel-digests",
        "ratatoskr-knowledge",
    ] {
        expect(
            pool,
            operation,
            &message(producer, Some(OWNER), operation),
            Outcome::Rejected,
        )
        .await;
    }

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn each_capture_producer_with_the_owners_tenant_is_applied() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for producer in [
        "ratatoskr-extractor",
        "ratatoskr-x",
        "ratatoskr-instagram",
        "ratatoskr-threads",
    ] {
        let operation = accepted(pool, "content.capture.submit").await;
        expect(
            pool,
            operation,
            &message(producer, Some(OWNER), operation),
            Outcome::Applied,
        )
        .await;
    }

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn channel_digest_operations_accept_only_the_channel_digests_producer() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for kind in [
        "channel_digest.run",
        "channel_digest.subscription.set",
        "channel_digest.schedule.occurrence",
    ] {
        let operation = accepted(pool, kind).await;
        expect(
            pool,
            operation,
            &message("ratatoskr-extractor", Some(OWNER), operation),
            Outcome::Rejected,
        )
        .await;
        expect(
            pool,
            operation,
            &message("ratatoskr-channel-digests", Some(OTHER), operation),
            Outcome::Rejected,
        )
        .await;
        expect(
            pool,
            operation,
            &message("ratatoskr-channel-digests", Some(OWNER), operation),
            Outcome::Applied,
        )
        .await;
    }

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_kind_with_no_row_is_rejected_for_every_producer() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted(pool, "github.sync.requested").await;

    for producer in [
        "ratatoskr-extractor",
        "ratatoskr-x",
        "ratatoskr-instagram",
        "ratatoskr-threads",
        "ratatoskr-channel-digests",
        "ratatoskr-chatgpt",
        "ratatoskr-claude",
        "ratatoskr-github",
        "ratatoskr-knowledge",
    ] {
        expect(
            pool,
            operation,
            &message(producer, Some(OWNER), operation),
            Outcome::Rejected,
        )
        .await;
    }

    harness.cleanup().await.expect("cleanup");
}

/// A report exactly as the two AI archive services publish it today: the envelope their
/// `operation_report_envelope` builds carries no `tenant_id`, and their outbox pump publishes the
/// stored document unchanged. The tenant of an AI-archive import is bound by Platform's own
/// acceptance record, so this shape must keep being applied (XR-021 R2-02, AI-archive amendment).
fn archive_service_message(provider: &str, operation_id: Uuid) -> Incoming {
    let producer = format!("ratatoskr-{provider}");
    let event_id = Uuid::now_v7();
    let envelope = serde_json::json!({
        "event_id": event_id,
        "event_type": "platform.operation.reported.v1",
        "occurred_at": "2023-11-14T22:13:20Z",
        "producer": producer,
        "aggregate_id": format!("operation:{operation_id}"),
        "correlation_id": format!("event:{event_id}"),
        "schema_version": 1,
        "payload": report(operation_id),
    });
    assert!(
        envelope.get("tenant_id").is_none(),
        "the AI archive services publish no tenant_id"
    );
    Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::parse(&format!("evt.ai-archive.{provider}.operation.reported.v1"))
            .expect("the provider report subject"),
        producer,
        payload: envelope,
    }
}

async fn accepted_import(pool: &sqlx::PgPool, provider: &str, binding_owner: Uuid) -> Uuid {
    let operation = accepted(pool, "ai_archive.import").await;
    platform_operations::record_ai_archive_acceptance(
        pool,
        &platform_operations::AiArchiveAcceptance {
            operation_id: operation,
            owner_user_id: binding_owner,
            device_id: Uuid::now_v7(),
            provider: provider.to_owned(),
            sha256: "00".repeat(32),
            byte_size: 1,
            accepted_at: now(),
        },
    )
    .await
    .expect("recording the archive binding");
    operation
}

#[tokio::test]
async fn a_bound_ai_archive_report_without_a_tenant_is_applied() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for provider in ["chatgpt", "claude"] {
        let operation = accepted_import(pool, provider, OWNER).await;
        expect(
            pool,
            operation,
            &archive_service_message(provider, operation),
            Outcome::Applied,
        )
        .await;
    }

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_bound_ai_archive_report_naming_another_tenant_is_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted_import(pool, "chatgpt", OWNER).await;

    let mut forged = archive_service_message("chatgpt", operation);
    forged.payload["tenant_id"] = serde_json::json!(format!("user:{OTHER}"));
    expect(pool, operation, &forged, Outcome::Rejected).await;
    forged.message_id = Uuid::now_v7();
    forged.payload["tenant_id"] = serde_json::json!("user:not-a-uuid");
    expect(pool, operation, &forged, Outcome::Rejected).await;

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn an_acceptance_record_for_another_owner_does_not_bind_the_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted_import(pool, "claude", OTHER).await;

    expect(
        pool,
        operation,
        &archive_service_message("claude", operation),
        Outcome::Rejected,
    )
    .await;

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn an_import_report_without_a_tenant_or_an_acceptance_record_is_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted(pool, "ai_archive.import").await;

    let mut unbound = message("ratatoskr-chatgpt", None, operation);
    expect(pool, operation, &unbound, Outcome::Rejected).await;
    unbound = archive_service_message("chatgpt", operation);
    expect(pool, operation, &unbound, Outcome::Rejected).await;

    harness.cleanup().await.expect("cleanup");
}

/// The quoted value on the line after `marker`, or the first quoted value of the line holding it.
fn literal_after(source: &str, marker: &str) -> Vec<String> {
    source
        .lines()
        .filter(|line| line.contains(marker))
        .filter_map(|line| line.split('"').nth(1).map(str::to_owned))
        .collect()
}

/// Every kind Platform mints, read from the sources that mint it. `platform_operations` cannot
/// depend on those crates, so the test reads their text: the `*_KIND` constants of the public API,
/// and `Target::operation_kind` of ingest, whose arms are `Self::X => "<kind>",`.
fn minted_kinds() -> Vec<String> {
    let mut kinds = Vec::new();
    for source in [
        include_str!("../../public-api/src/captures.rs"),
        include_str!("../../public-api/src/archives.rs"),
        include_str!("../../public-api/src/channel_digests.rs"),
    ] {
        for line in source.lines() {
            let line = line.trim();
            if line.starts_with("const ") && line.contains("KIND: &str = \"") {
                kinds.push(line.split('"').nth(1).expect("a quoted kind").to_owned());
            }
        }
    }
    let ingest = include_str!("../../ingest/src/lib.rs");
    let body = ingest
        .split("pub const fn operation_kind")
        .nth(1)
        .expect("Target::operation_kind");
    let body = body.split("\n    }\n").next().expect("the function body");
    kinds.extend(literal_after(body, "=> \""));
    kinds
}

#[test]
fn every_minted_operation_kind_has_an_authority_row() {
    let kinds = minted_kinds();
    for expected in [
        "content.capture.submit",
        "ai_archive.import",
        "channel_digest.subscription.set",
        "channel_digest.run",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "the source scan did not find {expected}: {kinds:?}"
        );
    }
    for kind in &kinds {
        assert!(
            producers_for(kind).is_some(),
            "{kind} is minted by Platform and has no row in the report authority table"
        );
    }
    // The schedule kind a service registers at runtime cannot be scanned from Platform's sources.
    assert!(producers_for("channel_digest.schedule.occurrence").is_some());
}

#[test]
fn the_ai_archive_import_row_defers_to_the_acceptance_binding() {
    assert_eq!(
        producers_for("ai_archive.import"),
        Some(&Producers::ProviderBound)
    );
    assert!(AUTHORITY.iter().all(|(kind, producers)| match producers {
        Producers::Listed(listed) => !listed.is_empty() && !kind.is_empty(),
        Producers::ProviderBound => *kind == "ai_archive.import",
    }));
}
