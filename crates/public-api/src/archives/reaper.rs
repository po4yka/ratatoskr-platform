//! Removing abandoned AI-archive staging (XR-021 CONTRACTS.md R5-04).

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use platform_telemetry::metrics::{
    PLATFORM_ARCHIVE_STAGING_DIRECTORIES, PLATFORM_ARCHIVE_STAGING_OLDEST_DIRECTORY_AGE_SECONDS,
    PLATFORM_ARCHIVE_STAGING_REMOVED_TOTAL,
};
use sqlx::{PgPool, Row as _};

use super::to_offset;

/// The bounds of one sweep.
#[derive(Debug, Clone, Copy)]
pub struct SweepLimits {
    /// The most transfer sessions one sweep reaps.
    pub max_sessions: usize,
    /// The most orphan entries one sweep removes.
    pub max_orphans: usize,
    /// How long an operation stays terminal before its transfer is reaped.
    pub terminal_grace: jiff::SignedDuration,
    /// How old an unowned `rst_*` directory or `.readiness-*` file must be before it is removed.
    pub orphan_age: jiff::SignedDuration,
}

impl Default for SweepLimits {
    fn default() -> Self {
        Self {
            max_sessions: 200,
            max_orphans: 200,
            terminal_grace: jiff::SignedDuration::from_secs(600),
            orphan_age: jiff::SignedDuration::from_secs(3600),
        }
    }
}

/// What one sweep removed and what it left.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Transfers reaped because they expired.
    pub expired: u64,
    /// Transfers reaped because their session failed.
    pub failed: u64,
    /// Transfers reaped because their session finalized.
    pub finalized: u64,
    /// Transfers reaped because their operation had been terminal for longer than the grace.
    pub terminal: u64,
    /// Entries removed because nothing owned them.
    pub orphan: u64,
    /// Bytes under the removed directories.
    pub bytes_freed: u64,
    /// Token directories left under the root.
    pub directories: u64,
    /// The age of the oldest of them, in seconds.
    pub oldest_directory_age_seconds: u64,
}

/// Why a transfer was reaped. The label set of `platform_archive_staging_removed_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Expired,
    Failed,
    Finalized,
    Terminal,
}

impl Reason {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "expired" => Some(Self::Expired),
            "failed" => Some(Self::Failed),
            "finalized" => Some(Self::Finalized),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

/// The first reason that applies, in the order of the contract: a session that failed or finalized
/// is dead whatever its clock says, then the clock, then the operation.
const CANDIDATES: &str = "
    select t.resumption_token,
           case when t.session_state = 'failed' then 'failed'
                when t.session_state = 'finalized' then 'finalized'
                when t.expires_at <= $1 then 'expired'
                else 'terminal' end as reason
      from operations.ai_archive_transfers t
      join operations.operations o using (operation_id)
     where t.session_state in ('failed', 'finalized')
        or t.expires_at <= $1
        or o.terminated_at <= $2
     order by t.opened_at, t.resumption_token
     limit $3";

/// Whether `name` is a token this service minted: `rst_` and 24 to 64 characters of `[0-9a-v]`.
fn is_token_name(name: &str) -> bool {
    name.strip_prefix("rst_").is_some_and(|rest| {
        (24..=64).contains(&rest.len())
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'v').contains(&byte))
    })
}

/// A directory the sweep may remove or count: a real directory, never a link to one.
fn is_real_directory(path: &Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_dir())
}

/// The bytes of regular files under `path`, without following a link.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let entry_path = entry.path();
            match std::fs::symlink_metadata(&entry_path) {
                Ok(metadata) if metadata.file_type().is_dir() => tree_bytes(&entry_path),
                Ok(metadata) if metadata.file_type().is_file() => metadata.len(),
                _ => 0,
            }
        })
        .sum()
}

/// Remove one entry of the root, returning the bytes freed, or `Ok(None)` when it is already gone.
///
/// A link is unlinked and never followed; `remove_dir_all` does the same for the entry itself.
fn remove_entry(path: &Path) -> std::io::Result<Option<u64>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_dir() {
        let bytes = tree_bytes(path);
        std::fs::remove_dir_all(path)?;
        Ok(Some(bytes))
    } else {
        let bytes = if metadata.file_type().is_file() {
            metadata.len()
        } else {
            0
        };
        std::fs::remove_file(path)?;
        Ok(Some(bytes))
    }
}

/// The modification time of `metadata` as an instant.
fn modified(metadata: &std::fs::Metadata) -> jiff::Timestamp {
    jiff::Timestamp::new(metadata.mtime(), 0).unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// Run one bounded sweep.
///
/// A transfer is reaped when its session `failed` or `finalized`, when it expired, or when its
/// operation has been terminal for longer than `limits.terminal_grace`; its directory is removed
/// first and its row deleted second, so a removal that fails leaves the row for the next sweep. A
/// directory named like a token with no row, and a stray `.readiness-*` file, go once older than
/// `limits.orphan_age`. Entries are read with `symlink_metadata` and a link is never followed.
///
/// # Errors
///
/// [`sqlx::Error`] if the candidate query fails. A filesystem failure on one entry is logged and
/// leaves that entry (and its row) for the next sweep.
pub async fn sweep_once(
    pool: &PgPool,
    root: &Path,
    now: jiff::Timestamp,
    limits: &SweepLimits,
) -> Result<SweepReport, sqlx::Error> {
    let mut report = SweepReport::default();
    reap_sessions(pool, root, now, limits, &mut report).await?;
    reap_orphans(pool, root, now, limits, &mut report).await?;
    let root = root.to_owned();
    let (directories, oldest) = tokio::task::spawn_blocking(move || survey(&root, now))
        .await
        .unwrap_or_default();
    report.directories = directories;
    report.oldest_directory_age_seconds = oldest;
    Ok(report)
}

async fn reap_sessions(
    pool: &PgPool,
    root: &Path,
    now: jiff::Timestamp,
    limits: &SweepLimits,
    report: &mut SweepReport,
) -> Result<(), sqlx::Error> {
    let terminal_before = now
        .checked_sub(limits.terminal_grace)
        .unwrap_or(jiff::Timestamp::MIN);
    let rows = sqlx::query(CANDIDATES)
        .bind(to_offset(now))
        .bind(to_offset(terminal_before))
        .bind(i64::try_from(limits.max_sessions).unwrap_or(i64::MAX))
        .fetch_all(pool)
        .await?;
    for row in rows {
        let (Ok(token), Ok(reason)) = (
            row.try_get::<String, _>("resumption_token"),
            row.try_get::<String, _>("reason"),
        ) else {
            continue;
        };
        let Some(reason) = Reason::parse(&reason) else {
            continue;
        };
        let path = root.join(&token);
        let removal = tokio::task::spawn_blocking(move || remove_entry(&path)).await;
        match removal {
            Ok(Ok(bytes)) => {
                report.bytes_freed = report.bytes_freed.saturating_add(bytes.unwrap_or(0));
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, token, "the staging directory of a dead transfer could not be removed");
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, token, "the staging removal task failed");
                continue;
            }
        }
        sqlx::query("delete from operations.ai_archive_transfers where resumption_token = $1")
            .bind(&token)
            .execute(pool)
            .await?;
        match reason {
            Reason::Expired => report.expired += 1,
            Reason::Failed => report.failed += 1,
            Reason::Finalized => report.finalized += 1,
            Reason::Terminal => report.terminal += 1,
        }
    }
    Ok(())
}

/// One entry of the root that no row owns and that is old enough to go.
struct Orphan {
    path: PathBuf,
    name: String,
    modified: jiff::Timestamp,
}

fn orphan_candidates(root: &Path, cutoff: jiff::Timestamp) -> Vec<Orphan> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<Orphan> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
            let kind = metadata.file_type();
            let eligible = (kind.is_dir() && is_token_name(&name))
                || (kind.is_file() && name.starts_with(".readiness-"));
            let modified = modified(&metadata);
            (eligible && modified < cutoff).then(|| Orphan {
                path: entry.path(),
                name,
                modified,
            })
        })
        .collect();
    found.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then(left.name.cmp(&right.name))
    });
    found
}

async fn reap_orphans(
    pool: &PgPool,
    root: &Path,
    now: jiff::Timestamp,
    limits: &SweepLimits,
    report: &mut SweepReport,
) -> Result<(), sqlx::Error> {
    let cutoff = now
        .checked_sub(limits.orphan_age)
        .unwrap_or(jiff::Timestamp::MIN);
    let scanned = root.to_owned();
    let candidates = tokio::task::spawn_blocking(move || orphan_candidates(&scanned, cutoff))
        .await
        .unwrap_or_default();
    if candidates.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = candidates
        .iter()
        .map(|orphan| orphan.name.clone())
        .collect();
    let owned: Vec<String> = sqlx::query_scalar(
        "select resumption_token from operations.ai_archive_transfers
          where resumption_token = any($1)",
    )
    .bind(&names)
    .fetch_all(pool)
    .await?;
    for orphan in candidates
        .into_iter()
        .filter(|orphan| !owned.contains(&orphan.name))
        .take(limits.max_orphans)
    {
        let path = orphan.path;
        match tokio::task::spawn_blocking(move || remove_entry(&path)).await {
            Ok(Ok(bytes)) => {
                report.bytes_freed = report.bytes_freed.saturating_add(bytes.unwrap_or(0));
                report.orphan += 1;
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, name = orphan.name, "an orphan staging entry could not be removed");
            }
            Err(error) => tracing::warn!(%error, "the orphan removal task failed"),
        }
    }
    Ok(())
}

/// How many token directories remain and how old the oldest is.
fn survey(root: &Path, now: jiff::Timestamp) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (0, 0);
    };
    let mut count = 0_u64;
    let mut oldest = 0_u64;
    for entry in entries.filter_map(Result::ok) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_token_name(&name) {
            continue;
        }
        let Some(metadata) = is_real_directory(&entry.path()) else {
            continue;
        };
        count += 1;
        let age = now
            .as_second()
            .saturating_sub(modified(&metadata).as_second());
        oldest = oldest.max(u64::try_from(age).unwrap_or(0));
    }
    (count, oldest)
}

impl SweepReport {
    /// Publish this sweep: the removals as a counter and the survey as two gauges.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a directory count and an age in seconds, exported as f64 gauges"
    )]
    pub fn record(&self) {
        for (reason, count) in [
            ("expired", self.expired),
            ("failed", self.failed),
            ("finalized", self.finalized),
            ("terminal", self.terminal),
            ("orphan", self.orphan),
        ] {
            metrics::counter!(PLATFORM_ARCHIVE_STAGING_REMOVED_TOTAL, "reason" => reason)
                .increment(count);
        }
        metrics::gauge!(PLATFORM_ARCHIVE_STAGING_DIRECTORIES).set(self.directories as f64);
        metrics::gauge!(PLATFORM_ARCHIVE_STAGING_OLDEST_DIRECTORY_AGE_SECONDS)
            .set(self.oldest_directory_age_seconds as f64);
    }
}
