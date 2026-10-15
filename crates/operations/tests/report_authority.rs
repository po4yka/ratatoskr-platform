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

/// An operation accepted for `reporter`, the lane Platform dispatched it to (XR-021 R3-02).
async fn accepted_by(pool: &sqlx::PgPool, kind: &str, reporter: Option<&str>) -> Uuid {
    platform_operations::accept(pool, OWNER, kind, CORRELATION, None, reporter, now())
        .await
        .expect("accepting")
        .operation_id
}

/// An operation accepted for the lane the kind's own intake path records: the extractor for a
/// capture, channel-digests for a channel digest, and none for a kind bound some other way.
async fn accepted(pool: &sqlx::PgPool, kind: &str) -> Uuid {
    let reporter = match kind {
        "content.capture.submit" => Some("ratatoskr-extractor"),
        "channel_digest.subscription.set"
        | "channel_digest.run"
        | "channel_digest.schedule.occurrence" => Some("ratatoskr-channel-digests"),
        "notes.connection.connect" => Some("ratatoskr-notes"),
        _ => None,
    };
    accepted_by(pool, kind, reporter).await
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
        let operation = accepted_by(pool, "content.capture.submit", Some(producer)).await;
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

/// XR-021 R3-02: the lane an operation was dispatched to is recorded, and only that lane reports.
#[tokio::test]
async fn an_x_report_for_an_instagram_capture_is_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted_by(pool, "content.capture.submit", Some("ratatoskr-instagram")).await;

    for other in ["ratatoskr-x", "ratatoskr-threads", "ratatoskr-extractor"] {
        expect(
            pool,
            operation,
            &message(other, Some(OWNER), operation),
            Outcome::Rejected,
        )
        .await;
    }

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_capture_report_from_its_recorded_reporter_is_applied() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted_by(pool, "content.capture.submit", Some("ratatoskr-instagram")).await;

    let stored = platform_operations::find(pool, operation)
        .await
        .expect("reading")
        .expect("the operation");
    assert_eq!(stored.reporter.as_deref(), Some("ratatoskr-instagram"));
    expect(
        pool,
        operation,
        &message("ratatoskr-instagram", Some(OWNER), operation),
        Outcome::Applied,
    )
    .await;

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_listed_kind_without_a_recorded_reporter_rejects_every_producer() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for (kind, producers) in AUTHORITY {
        let Producers::Listed(listed) = producers else {
            continue;
        };
        let operation = accepted_by(pool, kind, None).await;
        for producer in *listed {
            expect(
                pool,
                operation,
                &message(producer, Some(OWNER), operation),
                Outcome::Rejected,
            )
            .await;
        }
    }

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn accept_refuses_a_reporter_outside_the_kinds_list() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    for (kind, reporter) in [
        ("content.capture.submit", "ratatoskr-channel-digests"),
        ("channel_digest.run", "ratatoskr-extractor"),
        ("ai_archive.import", "ratatoskr-chatgpt"),
        ("github.sync.requested", "ratatoskr-github"),
    ] {
        let refused = platform_operations::accept(
            pool,
            OWNER,
            kind,
            CORRELATION,
            None,
            Some(reporter),
            now(),
        )
        .await;
        assert!(refused.is_err(), "{kind} accepted the reporter {reporter}");
    }
    let (count,): (i64,) = sqlx::query_as("select count(*) from operations.operations")
        .fetch_one(pool)
        .await
        .expect("counting");
    assert_eq!(count, 0, "a refused reporter must record no operation");

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

/// Workspace OBS-023 task 9.3: Edge mints a connect operation for the notes lane, and only that lane
/// reports on it.
#[tokio::test]
async fn notes_connect_accepts_only_the_notes_lane() {
    let kinds = minted_kinds();
    assert!(
        kinds.iter().any(|kind| kind == "notes.connection.connect"),
        "notes.connection.connect is not minted by Platform: {kinds:?}"
    );
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = accepted(pool, "notes.connection.connect").await;

    expect(
        pool,
        operation,
        &message("ratatoskr-x", Some(OWNER), operation),
        Outcome::Rejected,
    )
    .await;
    expect(
        pool,
        operation,
        &message("ratatoskr-notes", Some(OWNER), operation),
        Outcome::Applied,
    )
    .await;

    harness.cleanup().await.expect("cleanup");
}

/// Workspace OBS-024 task 10.5: Edge mints a notes write and a write-mode change for the notes
/// lane, and only that lane reports on them.
#[tokio::test]
async fn notes_write_kinds_accept_only_notes_reports() {
    let kinds = minted_kinds();
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    for kind in ["notes.file.write", "notes.connection.write_mode"] {
        assert!(
            kinds.iter().any(|minted| minted == kind),
            "{kind} is not minted by Platform: {kinds:?}"
        );
        let accepted = platform_operations::accept(
            pool,
            OWNER,
            kind,
            CORRELATION,
            None,
            Some("ratatoskr-notes"),
            now(),
        )
        .await;
        assert!(
            accepted.is_ok(),
            "{kind} refuses the notes lane: {:?}",
            accepted.err()
        );
        let operation = accepted.expect("accepted").operation_id;

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
            &message("ratatoskr-notes", Some(OWNER), operation),
            Outcome::Applied,
        )
        .await;
    }

    harness.cleanup().await.expect("cleanup");
}

/// Workspace SITE-025 task 6.10: Edge mints a site release build for the sites lane, and only that
/// lane reports on it (spec `site-publications`, "Another identity reports for a build").
#[tokio::test]
async fn sites_build_accepts_only_the_sites_lane() {
    const KIND: &str = "sites.release.build";
    let kinds = minted_kinds();
    assert!(
        kinds.iter().any(|minted| minted == KIND),
        "{KIND} is not minted by Platform: {kinds:?}"
    );
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let accepted = platform_operations::accept(
        pool,
        OWNER,
        KIND,
        CORRELATION,
        None,
        Some("ratatoskr-sites"),
        now(),
    )
    .await;
    assert!(
        accepted.is_ok(),
        "{KIND} refuses the sites lane: {:?}",
        accepted.err()
    );
    let operation = accepted.expect("accepted").operation_id;

    expect(
        pool,
        operation,
        &message("ratatoskr-notes", Some(OWNER), operation),
        Outcome::Rejected,
    )
    .await;
    expect(
        pool,
        operation,
        &message("ratatoskr-sites", Some(OWNER), operation),
        Outcome::Applied,
    )
    .await;

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
        include_str!("../../public-api/src/notes.rs"),
        include_str!("../../public-api/src/notes_writes.rs"),
        include_str!("../../public-api/src/site_releases.rs"),
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
        "notes.connection.connect",
        "notes.file.write",
        "notes.connection.write_mode",
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

/// The arguments of every `platform_operations::accept(` call in `source`, split at the top level.
fn accept_calls(source: &str) -> Vec<Vec<String>> {
    let key = "platform_operations::accept(";
    let mut calls = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find(key) {
        let body = rest.get(at + key.len()..).unwrap_or_default();
        let (mut depth, mut args, mut current) = (1_usize, Vec::new(), String::new());
        for character in body.chars() {
            match character {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                ',' if depth == 1 => {
                    args.push(current.trim().to_owned());
                    current.clear();
                    continue;
                }
                _ => {}
            }
            current.push(character);
        }
        if !current.trim().is_empty() {
            args.push(current.trim().to_owned());
        }
        calls.push(args);
        rest = body;
    }
    calls
}

/// XR-021 R3-02: a mint site that passes no lane for a kind with a listed allow-list creates an
/// operation nobody may report for. The scan is textual (the mint sites live in crates this one
/// cannot depend on): it finds the reporter argument (the sixth) of each production `accept` call,
/// and the `reporter:` member of each route's `Intake` literal. It cannot prove the value is the
/// right lane; the behavioural tests of each intake path do that.
#[test]
fn every_mint_site_passes_a_lane_for_a_listed_kind() {
    for (name, source) in [
        (
            "public-api intake",
            include_str!("../../public-api/src/intake.rs"),
        ),
        (
            "ingest webhook",
            include_str!("../../ingest/src/webhook.rs"),
        ),
        ("scheduler", include_str!("../../scheduling/src/lib.rs")),
        (
            "notes connect",
            include_str!("../../public-api/src/notes.rs"),
        ),
    ] {
        let calls = accept_calls(source);
        assert!(!calls.is_empty(), "{name} has no accept call to scan");
        for args in calls {
            assert_eq!(
                args.len(),
                7,
                "{name}: accept takes seven arguments: {args:?}"
            );
            assert_ne!(args[5], "None", "{name} mints without a lane");
        }
    }
    // AI-archive operations are bound by their acceptance record and record no lane.
    let archives = accept_calls(include_str!("../../public-api/src/archives.rs"));
    assert_eq!(archives.len(), 1);
    assert_eq!(archives[0][5], "None");

    for (name, source) in [
        ("captures", include_str!("../../public-api/src/captures.rs")),
        (
            "channel digests",
            include_str!("../../public-api/src/channel_digests.rs"),
        ),
        (
            "notes writes",
            include_str!("../../public-api/src/notes_writes.rs"),
        ),
        (
            "site releases",
            include_str!("../../public-api/src/site_releases.rs"),
        ),
    ] {
        let literals = source.matches("Intake {").count();
        assert!(literals > 0, "{name} builds no Intake to scan");
        assert_eq!(
            source.matches("reporter: ").count(),
            literals,
            "{name}: every Intake literal names its reporter"
        );
        assert!(
            !source.contains("reporter: None"),
            "{name} passes no lane for a listed kind"
        );
    }
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
