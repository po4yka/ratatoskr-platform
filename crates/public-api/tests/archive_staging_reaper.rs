//! The staging reaper against a real `PostgreSQL` and a temporary staging root (XR-021 R5-04).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::archives::reaper::{SweepLimits, sweep_once};
use uuid::Uuid;

/// The instant every fixture is positioned against, so no assertion reads the wall clock.
fn base() -> Timestamp {
    "2026-10-12T12:00:00Z".parse().expect("a fixed instant")
}

fn ago(seconds: i64) -> Timestamp {
    base()
        .checked_sub(SignedDuration::from_secs(seconds))
        .expect("an instant")
}

fn later(seconds: i64) -> Timestamp {
    base()
        .checked_add(SignedDuration::from_secs(seconds))
        .expect("an instant")
}

fn offset(value: Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond()).expect("an offset")
}

fn stamp_directory(path: &Path, at: Timestamp) {
    std::fs::File::open(path)
        .expect("the directory")
        .set_modified(at.into())
        .expect("a modification time");
}

fn token() -> String {
    format!("rst_{}", Uuid::now_v7().simple())
}

/// One transfer row to seed and what surrounds it.
struct Seed {
    session_state: &'static str,
    expires_at: Timestamp,
    operation_status: &'static str,
    terminated_at: Option<Timestamp>,
    opened_at: Timestamp,
    directory: bool,
}

impl Seed {
    /// An open, unexpired transfer of a running operation: nothing may touch it.
    fn live() -> Self {
        Self {
            session_state: "open",
            expires_at: later(86_400),
            operation_status: "running",
            terminated_at: None,
            opened_at: ago(60),
            directory: true,
        }
    }
}

async fn seed(pool: &sqlx::PgPool, root: &Path, spec: &Seed) -> String {
    let operation_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let accepted = ago(7200);
    sqlx::query(
        "insert into operations.operations
             (operation_id, owner_user_id, kind, status, correlation_id, accepted_at,
              status_changed_at, terminated_at)
         values ($1, $2, 'ai_archive.import', $3, 'req:reaper', $4, $4, $5)",
    )
    .bind(operation_id)
    .bind(owner)
    .bind(spec.operation_status)
    .bind(offset(accepted))
    .bind(spec.terminated_at.map(offset))
    .execute(pool)
    .await
    .expect("an operation");
    sqlx::query(
        "insert into operations.ai_archive_acceptances
             (operation_id, owner_user_id, device_id, provider, sha256, byte_size, accepted_at)
         values ($1, $2, $3, 'chatgpt', $4, 65537, $5)",
    )
    .bind(operation_id)
    .bind(owner)
    .bind(Uuid::now_v7())
    .bind("a".repeat(64))
    .bind(offset(accepted))
    .execute(pool)
    .await
    .expect("an acceptance");
    let token = token();
    sqlx::query(
        "insert into operations.ai_archive_transfers
             (resumption_token, operation_id, declared_size_bytes, media_type, digest_sha256,
              chunk_size_bytes, expected_chunks, session_state, expires_at, opened_at)
         values ($1, $2, 65537, 'application/zip', $3, 65536, 2, $4, $5, $6)",
    )
    .bind(&token)
    .bind(operation_id)
    .bind("a".repeat(64))
    .bind(spec.session_state)
    .bind(offset(spec.expires_at))
    .bind(offset(spec.opened_at))
    .execute(pool)
    .await
    .expect("a transfer");
    if spec.directory {
        let directory = root.join(&token);
        std::fs::create_dir_all(&directory).expect("a token directory");
        std::fs::write(directory.join("0.chunk"), vec![b'x'; 1000]).expect("a chunk");
    }
    token
}

async fn has_row(pool: &sqlx::PgPool, token: &str) -> bool {
    sqlx::query_scalar(
        "select exists (select 1 from operations.ai_archive_transfers where resumption_token = $1)",
    )
    .bind(token)
    .fetch_one(pool)
    .await
    .expect("a row count")
}

fn root(parent: &Path) -> PathBuf {
    let root = parent.join("staging");
    std::fs::create_dir_all(&root).expect("a staging root");
    root
}

#[tokio::test]
async fn each_reason_removes_exactly_its_directory_and_row() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let pool = harness.pool();

    let expired = seed(
        pool,
        &root,
        &Seed {
            expires_at: ago(10),
            opened_at: ago(86_410),
            ..Seed::live()
        },
    )
    .await;
    let failed = seed(
        pool,
        &root,
        &Seed {
            session_state: "failed",
            ..Seed::live()
        },
    )
    .await;
    let finalized = seed(
        pool,
        &root,
        &Seed {
            session_state: "finalized",
            ..Seed::live()
        },
    )
    .await;
    let terminal = seed(
        pool,
        &root,
        &Seed {
            operation_status: "succeeded",
            terminated_at: Some(ago(601)),
            ..Seed::live()
        },
    )
    .await;
    let live = seed(pool, &root, &Seed::live()).await;
    let just_terminal = seed(
        pool,
        &root,
        &Seed {
            operation_status: "failed",
            terminated_at: Some(ago(599)),
            ..Seed::live()
        },
    )
    .await;

    let report = sweep_once(pool, &root, base(), &SweepLimits::default())
        .await
        .expect("a sweep");

    assert_eq!(
        (
            report.expired,
            report.failed,
            report.finalized,
            report.terminal,
            report.orphan
        ),
        (1, 1, 1, 1, 0),
        "{report:?}"
    );
    assert_eq!(report.bytes_freed, 4000, "four 1000 byte chunks");
    for gone in [&expired, &failed, &finalized, &terminal] {
        assert!(!root.join(gone).exists(), "{gone} directory removed");
        assert!(!has_row(pool, gone).await, "{gone} row deleted");
    }
    for kept in [&live, &just_terminal] {
        assert!(root.join(kept).is_dir(), "{kept} directory kept");
        assert!(has_row(pool, kept).await, "{kept} row kept");
    }
    assert_eq!(report.directories, 2);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn an_open_unexpired_session_of_a_live_operation_is_never_touched() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let live = seed(harness.pool(), &root, &Seed::live()).await;

    let report = sweep_once(harness.pool(), &root, base(), &SweepLimits::default())
        .await
        .expect("a sweep");

    assert_eq!(
        (
            report.expired,
            report.failed,
            report.finalized,
            report.terminal,
            report.orphan
        ),
        (0, 0, 0, 0, 0)
    );
    assert!(root.join(&live).is_dir());
    assert!(has_row(harness.pool(), &live).await);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn an_orphan_is_removed_only_after_its_grace() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let young = root.join(token());
    let old = root.join(token());
    let stray_young = root.join(format!(".readiness-{}", Uuid::now_v7()));
    let stray_old = root.join(format!(".readiness-{}", Uuid::now_v7()));
    let unrelated = root.join("lost+found");
    for directory in [&young, &old, &unrelated] {
        std::fs::create_dir_all(directory).expect("a directory");
        std::fs::write(directory.join("0.chunk"), b"abc").expect("a chunk");
    }
    for file in [&stray_young, &stray_old] {
        std::fs::write(file, b"").expect("a probe file");
    }
    stamp_directory(&young, ago(3599));
    stamp_directory(&old, ago(3601));
    stamp_directory(&unrelated, ago(900_000));
    stamp_directory(&stray_young, ago(3599));
    stamp_directory(&stray_old, ago(3601));

    let report = sweep_once(harness.pool(), &root, base(), &SweepLimits::default())
        .await
        .expect("a sweep");

    assert_eq!(report.orphan, 2, "{report:?}");
    assert!(young.is_dir(), "an orphan younger than the grace survives");
    assert!(!old.exists(), "an orphan older than the grace is removed");
    assert!(stray_young.is_file());
    assert!(!stray_old.exists(), "a stale readiness probe is removed");
    assert!(
        unrelated.is_dir(),
        "a name outside the token grammar is never touched"
    );
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_symbolic_link_named_like_a_token_is_not_followed() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let outside = parent.path().join("outside");
    std::fs::create_dir_all(&outside).expect("a directory outside the root");
    std::fs::write(outside.join("precious"), b"keep").expect("a file");
    let link = root.join(token());
    std::os::unix::fs::symlink(&outside, &link).expect("a symlink");
    // A row that points at the link, so the row-driven path meets it too.
    let linked_failed = seed(
        harness.pool(),
        &root,
        &Seed {
            session_state: "failed",
            directory: false,
            ..Seed::live()
        },
    )
    .await;
    std::os::unix::fs::symlink(&outside, root.join(&linked_failed)).expect("a second symlink");

    sweep_once(
        harness.pool(),
        &root,
        later(100_000),
        &SweepLimits::default(),
    )
    .await
    .expect("a sweep");

    assert!(
        outside.join("precious").is_file(),
        "nothing outside the root is touched"
    );
    assert!(
        link.symlink_metadata().is_ok(),
        "an orphan symlink is left alone, not followed and not unlinked"
    );
    assert!(
        !has_row(harness.pool(), &linked_failed).await,
        "the row of a symlinked entry is still deleted"
    );
    assert!(
        root.join(&linked_failed).symlink_metadata().is_err(),
        "only the link itself is unlinked, never its target"
    );
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn four_hundred_and_fifty_candidates_take_three_sweeps() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let mut tokens = Vec::new();
    for index in 0..450_i64 {
        tokens.push(
            seed(
                harness.pool(),
                &root,
                &Seed {
                    session_state: "failed",
                    // Oldest first: the first seeded are the first reaped.
                    opened_at: ago(100_000 - index),
                    ..Seed::live()
                },
            )
            .await,
        );
    }

    let mut removed = Vec::new();
    for _ in 0..3 {
        let report = sweep_once(harness.pool(), &root, base(), &SweepLimits::default())
            .await
            .expect("a sweep");
        removed.push(report.failed);
    }

    assert_eq!(removed, vec![200, 200, 50]);
    let first = tokens.first().expect("a token");
    assert!(!has_row(harness.pool(), first).await);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn the_oldest_two_hundred_go_first() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let oldest = seed(
        harness.pool(),
        &root,
        &Seed {
            session_state: "failed",
            opened_at: ago(500_000),
            ..Seed::live()
        },
    )
    .await;
    let mut newest = Vec::new();
    for index in 0..201_i64 {
        newest.push(
            seed(
                harness.pool(),
                &root,
                &Seed {
                    session_state: "failed",
                    opened_at: ago(1000 - index),
                    ..Seed::live()
                },
            )
            .await,
        );
    }

    sweep_once(harness.pool(), &root, base(), &SweepLimits::default())
        .await
        .expect("a sweep");

    assert!(
        !has_row(harness.pool(), &oldest).await,
        "the oldest is reaped in the first sweep"
    );
    let survivors =
        sqlx::query_scalar::<_, i64>("select count(*) from operations.ai_archive_transfers")
            .fetch_one(harness.pool())
            .await
            .expect("a count");
    assert_eq!(survivors, 202 - 200);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_reaped_row_whose_directory_is_missing_is_deleted_without_error() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let token = seed(
        harness.pool(),
        &root,
        &Seed {
            session_state: "failed",
            directory: false,
            ..Seed::live()
        },
    )
    .await;

    let report = sweep_once(harness.pool(), &root, base(), &SweepLimits::default())
        .await
        .expect("a missing directory is not an error");

    assert_eq!(report.failed, 1);
    assert!(!has_row(harness.pool(), &token).await);
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn the_directory_is_removed_before_the_row() {
    let harness = TestDatabase::create().await.expect("a test database");
    let parent = tempfile::tempdir().expect("a temporary directory");
    let root = root(parent.path());
    let token = seed(
        harness.pool(),
        &root,
        &Seed {
            session_state: "failed",
            ..Seed::live()
        },
    )
    .await;
    // A read-only root cannot lose an entry, which is a real failing remove and not a mock.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).expect("read-only");

    let blocked = sweep_once(harness.pool(), &root, base(), &SweepLimits::default()).await;

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("writable");
    let blocked = blocked.expect("a sweep reports a removal failure instead of failing");
    assert_eq!(blocked.failed, 0, "nothing was removed: {blocked:?}");
    assert!(root.join(&token).is_dir(), "the directory could not go");
    assert!(
        has_row(harness.pool(), &token).await,
        "the row stays until its directory is gone"
    );

    let recovered = sweep_once(harness.pool(), &root, base(), &SweepLimits::default())
        .await
        .expect("a sweep");
    assert_eq!(recovered.failed, 1);
    assert!(!root.join(&token).exists());
    assert!(!has_row(harness.pool(), &token).await);
    harness.cleanup().await.expect("cleanup");
}
