//! The staging root as a dependency of Edge: its readiness and the loop that sweeps it
//! (XR-021 CONTRACTS.md R5-04).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use platform_http::RuntimeState;
use sqlx::PgPool;

use super::reaper::{SweepLimits, sweep_once};

/// What the staging loop needs to know.
#[derive(Debug, Clone)]
pub struct StagingSettings {
    /// The staging root.
    pub root: PathBuf,
    /// The archive ceiling; the volume must hold twice this much to be ready.
    pub max_archive_bytes: u64,
    /// The time between two sweeps.
    pub interval: Duration,
    /// The bounds of one sweep.
    pub limits: SweepLimits,
}

/// The free space of the volume holding `path`, in bytes available to this process.
///
/// # Errors
///
/// The error of the underlying `statvfs` call.
pub fn volume_free_bytes(path: &Path) -> std::io::Result<u64> {
    fs4::available_space(path)
}

/// Whether Edge can create, sync and remove a file under `root`, creating `root` if needed.
pub async fn root_is_writable(root: &Path) -> bool {
    if tokio::fs::create_dir_all(root).await.is_err() {
        return false;
    }
    let probe = root.join(format!(".readiness-{}", uuid::Uuid::now_v7()));
    match tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
        .await
    {
        Ok(file) => {
            let synced = file.sync_all().await.is_ok();
            drop(file);
            let removed = tokio::fs::remove_file(probe).await.is_ok();
            synced && removed
        }
        Err(_) => false,
    }
}

/// Whether Edge may accept archive bytes into `root`: it is writable and the volume has at least
/// twice `max_archive_bytes` available, because Edge keeps the chunks and the assembled copy of an
/// archive until its receiver has the bytes. A volume that cannot be measured is not ready.
pub async fn staging_is_ready<F>(root: &Path, max_archive_bytes: u64, free_bytes: &F) -> bool
where
    F: Fn(&Path) -> std::io::Result<u64> + Send + Sync,
{
    if !root_is_writable(root).await {
        return false;
    }
    match free_bytes(root) {
        Ok(free) => free >= max_archive_bytes.saturating_mul(2),
        Err(error) => {
            tracing::warn!(%error, "the free space of the archive staging volume could not be read");
            false
        }
    }
}

/// Sweep the staging root every `settings.interval`, forever, and keep the staging readiness of
/// `health` true to what the volume can do.
///
/// A failed sweep is logged and the loop continues: a gap in the series is better than a reaper
/// that stopped because the database blinked.
pub async fn run<F>(
    pool: PgPool,
    health: Arc<RuntimeState>,
    settings: StagingSettings,
    free_bytes: F,
) where
    F: Fn(&Path) -> std::io::Result<u64> + Send + Sync + 'static,
{
    let mut ticker = tokio::time::interval(settings.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        health.set_archive_staging_ready(
            staging_is_ready(&settings.root, settings.max_archive_bytes, &free_bytes).await,
        );
        match sweep_once(
            &pool,
            &settings.root,
            jiff::Timestamp::now(),
            &settings.limits,
        )
        .await
        {
            Ok(report) => {
                report.record();
                let removed = report.expired
                    + report.failed
                    + report.finalized
                    + report.terminal
                    + report.orphan;
                if removed > 0 {
                    tracing::info!(
                        expired = report.expired,
                        failed = report.failed,
                        finalized = report.finalized,
                        terminal = report.terminal,
                        orphan = report.orphan,
                        bytes_freed = report.bytes_freed,
                        "archive staging sweep",
                    );
                }
            }
            Err(error) => tracing::warn!(%error, "an archive staging sweep failed"),
        }
    }
}
