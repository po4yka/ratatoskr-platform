//! Edge projects the conflict events of `ratatoskr-notes` onto the `notes.file.write` operations
//! (workspace change `add-notes-write-back`, task 10.8): a client that follows a write finds the
//! conflict it raised, or the conflict that it resolved, as a result reference of the operation.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_eventing::inbox::Outcome;
use platform_eventing::{Incoming, Subject, deliver};
use platform_operations::NotesConflictProjection;
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";
const OWNER: Uuid = Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0c01);
const OTHER: Uuid = Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0c02);
const CONNECTION: &str = "018f0000-0000-7000-8000-0000000000b1";
const HASH: &str = "0a1b2c3d4e5f60710a1b2c3d4e5f60710a1b2c3d4e5f60710a1b2c3d4e5f6071";

fn now() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).expect("a fixed timestamp")
}

fn raised(operation_id: Uuid, conflict_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "conflict_id": conflict_id,
        "operation_id": operation_id,
        "connection_id": CONNECTION,
        "path": "Projects/Plan.md",
        "origin": "platform",
        "media_type": "text/markdown",
        "base_hash": HASH,
    })
}

fn resolved(operation_id: Uuid, conflict_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "conflict_id": conflict_id,
        "operation_id": operation_id,
        "connection_id": CONNECTION,
        "path": "Projects/Plan.md",
        "origin": "platform",
        "revision": HASH,
    })
}

/// A conflict event in the envelope that `producer` publishes for `tenant`.
fn event(subject: &str, producer: &str, tenant: Uuid, payload: serde_json::Value) -> Incoming {
    let mut envelope = serde_json::json!({
        "event_id": Uuid::now_v7(),
        "producer": producer,
        "tenant_id": format!("user:{tenant}"),
    });
    envelope["payload"] = payload;
    Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::parse(subject).expect("a conflict subject"),
        producer: producer.to_owned(),
        payload: envelope,
    }
}

async fn write_operation(pool: &sqlx::PgPool, kind: &str) -> Uuid {
    platform_operations::accept(
        pool,
        OWNER,
        kind,
        CORRELATION,
        None,
        Some("ratatoskr-notes"),
        now(),
    )
    .await
    .expect("accepting")
    .operation_id
}

async fn results(pool: &sqlx::PgPool, operation_id: Uuid) -> Vec<(String, String)> {
    platform_operations::snapshot(pool, operation_id)
        .await
        .expect("the snapshot")
        .results
        .iter()
        .map(|result| {
            (
                String::from(result.result_kind.clone()),
                String::from(result.target.clone()),
            )
        })
        .collect()
}

async fn delivered(pool: &sqlx::PgPool, incoming: &Incoming) -> Option<Outcome> {
    deliver(pool, &NotesConflictProjection, incoming, now())
        .await
        .expect("delivering")
}

#[tokio::test]
async fn a_conflict_is_a_result_of_its_write_and_of_its_resolution() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let write = write_operation(pool, "notes.file.write").await;
    let resolution = write_operation(pool, "notes.file.write").await;
    let conflict = Uuid::now_v7();
    let target = format!("notes_conflict:{conflict}");

    let first = event(
        "evt.notes.conflict.raised.v1",
        "ratatoskr-notes",
        OWNER,
        raised(write, conflict),
    );
    assert_eq!(delivered(pool, &first).await, Some(Outcome::Applied));
    let again = event(
        "evt.notes.conflict.raised.v1",
        "ratatoskr-notes",
        OWNER,
        raised(write, conflict),
    );
    assert_eq!(delivered(pool, &again).await, Some(Outcome::Duplicate));
    assert_eq!(
        results(pool, write).await,
        [("notes.conflict.raised".to_owned(), target.clone())]
    );

    let closed = event(
        "evt.notes.conflict.resolved.v1",
        "ratatoskr-notes",
        OWNER,
        resolved(resolution, conflict),
    );
    assert_eq!(delivered(pool, &closed).await, Some(Outcome::Applied));
    assert_eq!(
        results(pool, resolution).await,
        [("notes.conflict.resolved".to_owned(), target)]
    );

    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_conflict_from_another_lane_tenant_or_kind_is_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let write = write_operation(pool, "notes.file.write").await;
    let connect = write_operation(pool, "notes.connection.connect").await;
    let raised_on = |operation| raised(operation, Uuid::now_v7());

    for incoming in [
        event(
            "evt.notes.conflict.raised.v1",
            "ratatoskr-extractor",
            OWNER,
            raised_on(write),
        ),
        event(
            "evt.notes.conflict.raised.v1",
            "ratatoskr-notes",
            OTHER,
            raised_on(write),
        ),
        event(
            "evt.notes.conflict.raised.v1",
            "ratatoskr-notes",
            OWNER,
            raised_on(connect),
        ),
        event(
            "evt.notes.conflict.raised.v1",
            "ratatoskr-notes",
            OWNER,
            raised_on(Uuid::now_v7()),
        ),
        // Content in a member that the contract does not declare.
        event("evt.notes.conflict.raised.v1", "ratatoskr-notes", OWNER, {
            let mut payload = raised_on(write);
            payload["text"] = serde_json::json!("# Plan");
            payload
        }),
    ] {
        assert_eq!(
            delivered(pool, &incoming).await,
            Some(Outcome::Rejected),
            "{incoming:?}"
        );
    }
    assert!(results(pool, write).await.is_empty());
    assert!(results(pool, connect).await.is_empty());

    harness.cleanup().await.expect("cleanup");
}

/// A producer can name every stale member of a write group after the consumer expands.
#[tokio::test]
async fn a_group_conflict_with_stale_paths_is_applied() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let write = write_operation(pool, "notes.file.write").await;
    let conflict = Uuid::now_v7();
    let mut payload = raised(write, conflict);
    payload["stale_paths"] = serde_json::json!(["Projects/Plan.md", "Index.md"]);
    let incoming = event(
        "evt.notes.conflict.raised.v1",
        "ratatoskr-notes",
        OWNER,
        payload.clone(),
    );
    assert_eq!(delivered(pool, &incoming).await, Some(Outcome::Applied));
    assert_eq!(
        results(pool, write).await,
        [(
            "notes.conflict.raised".to_owned(),
            format!("notes_conflict:{conflict}")
        )]
    );
    assert_eq!(delivered(pool, &incoming).await, None);
    let again = event(
        "evt.notes.conflict.raised.v1",
        "ratatoskr-notes",
        OWNER,
        payload,
    );
    assert_eq!(delivered(pool, &again).await, Some(Outcome::Duplicate));
    assert_eq!(results(pool, write).await.len(), 1);
    harness.cleanup().await.expect("cleanup");
}
