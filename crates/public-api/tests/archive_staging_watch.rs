//! The loop Edge runs over the staging root: the reaper task and the staging readiness
//! (XR-021 R5-04 rules 1 and 4).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use platform_core::RuntimeRole;
use platform_http::RuntimeState;
use platform_persistence::test_support::TestDatabase;
use platform_public_api::archives::reaper::SweepLimits;
use platform_public_api::archives::staging::{StagingSettings, run, staging_is_ready};
use uuid::Uuid;

const CEILING: u64 = 1_048_576;

fn health() -> Arc<RuntimeState> {
    let health = Arc::new(RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    health.set_archive_receipt_ready("chatgpt", true);
    health.set_archive_report_ready("chatgpt", true);
    health
}

fn settings(root: &Path) -> StagingSettings {
    StagingSettings {
        root: root.to_owned(),
        max_archive_bytes: CEILING,
        // The sweep interval of the configuration is bounded below by rule V23; this constructor is
        // the test seam that lets the real loop be observed in seconds.
        interval: Duration::from_secs(1),
        limits: SweepLimits::default(),
    }
}

/// Poll `check` until it holds or five seconds pass.
async fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..50 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{what} did not happen within five seconds");
}

#[tokio::test]
async fn readiness_is_false_below_twice_the_ceiling_and_true_at_it() {
    let root = tempfile::tempdir().expect("a root");
    let free = AtomicU64::new(2 * CEILING - 1);
    let probe = |_: &Path| Ok(free.load(Ordering::Acquire));

    assert!(
        !staging_is_ready(root.path(), CEILING, &probe).await,
        "one byte short of twice the ceiling is not ready"
    );
    free.store(2 * CEILING, Ordering::Release);
    assert!(
        staging_is_ready(root.path(), CEILING, &probe).await,
        "exactly twice the ceiling is ready"
    );
    let failing = |_: &Path| Err(std::io::Error::other("statvfs failed"));
    assert!(
        !staging_is_ready(root.path(), CEILING, &failing).await,
        "a volume that cannot be measured is not ready"
    );
}

#[tokio::test]
async fn the_loop_flips_readiness_with_the_root_and_the_volume() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = parent.path().join("staging");
    std::fs::create_dir_all(&root).expect("a staging root");
    let health = health();
    health.set_archive_staging_ready(false);
    let free = Arc::new(AtomicU64::new(u64::MAX));
    let probe_free = Arc::clone(&free);
    let task = tokio::spawn(run(
        harness.pool().clone(),
        Arc::clone(&health),
        settings(&root),
        move |_: &Path| Ok(probe_free.load(Ordering::Acquire)),
    ));

    eventually("the loop reports a writable root ready", || {
        health.archive_provider_ready("chatgpt")
    })
    .await;

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).expect("read-only");
    eventually("an unwritable root turns readiness false", || {
        !health.archive_provider_ready("chatgpt")
    })
    .await;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("writable");
    eventually("a recovered root turns readiness true", || {
        health.archive_provider_ready("chatgpt")
    })
    .await;

    free.store(2 * CEILING - 1, Ordering::Release);
    eventually("a nearly full volume turns readiness false", || {
        !health.archive_provider_ready("chatgpt")
    })
    .await;
    free.store(2 * CEILING, Ordering::Release);
    eventually("a volume with room turns readiness true", || {
        health.archive_provider_ready("chatgpt")
    })
    .await;

    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn the_loop_removes_an_expired_fixture_within_two_intervals() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = parent.path().join("staging");
    std::fs::create_dir_all(&root).expect("a staging root");
    let operation_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let token = format!("rst_{}", Uuid::now_v7().simple());
    // A fixed instant years in the past is expired under any retention the loop applies.
    let accepted = time::OffsetDateTime::from_unix_timestamp(1_577_836_800).expect("2020-01-01");
    for statement in [
        "insert into operations.operations
             (operation_id, owner_user_id, kind, status, correlation_id, accepted_at, status_changed_at)
         values ($1, $2, 'ai_archive.import', 'running', 'req:watch', $3, $3)",
        "insert into operations.ai_archive_acceptances
             (operation_id, owner_user_id, device_id, provider, sha256, byte_size, accepted_at)
         select $1, $2, gen_random_uuid(), 'chatgpt', repeat('a', 64), 65537, $3",
    ] {
        sqlx::query(statement)
            .bind(operation_id)
            .bind(owner)
            .bind(accepted)
            .execute(harness.pool())
            .await
            .expect("a fixture row");
    }
    sqlx::query(
        "insert into operations.ai_archive_transfers
             (resumption_token, operation_id, declared_size_bytes, media_type, digest_sha256,
              chunk_size_bytes, expected_chunks, expires_at, opened_at)
         values ($1, $2, 65537, 'application/zip', repeat('a', 64), 65536, 2,
                 now() - interval '1 day', now() - interval '2 days')",
    )
    .bind(&token)
    .bind(operation_id)
    .execute(harness.pool())
    .await
    .expect("an expired transfer");
    std::fs::create_dir_all(root.join(&token)).expect("a token directory");
    std::fs::write(root.join(&token).join("0.chunk"), b"abc").expect("a chunk");
    let health = health();
    let task = tokio::spawn(run(
        harness.pool().clone(),
        Arc::clone(&health),
        settings(&root),
        |_: &Path| Ok(u64::MAX),
    ));

    eventually("the expired fixture directory is removed", || {
        !root.join(&token).exists()
    })
    .await;
    let rows: i64 = sqlx::query_scalar("select count(*) from operations.ai_archive_transfers")
        .fetch_one(harness.pool())
        .await
        .expect("a count");
    assert_eq!(rows, 0, "the row goes with the directory");

    task.abort();
    harness.cleanup().await.expect("cleanup");
}
