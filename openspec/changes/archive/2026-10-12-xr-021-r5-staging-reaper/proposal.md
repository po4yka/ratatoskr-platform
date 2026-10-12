## Why

Round 4 native smokes found that AI-archive upload staging is never cleaned. A client that stops mid-upload, a digest-mismatch session, a stale-reaped operation, an expired session nobody reopens and a crash between the row insert and the directory creation each leave up to twice the archive size under the staging root forever, and the comments that promise a "staging sweep" describe code that does not exist. The staging readiness flag is computed once at boot, so a full or unmounted volume is never noticed, and `open_transfer` replays the stored token of a `failed` session, which answers 404 on every later call.

Cites XR-021 CONTRACTS.md R5-04.

## What Changes

- A library function `platform_public_api::archives::reaper::sweep_once` removes the token directory and then the row of every transfer that expired, failed, left a finalized directory behind, or belongs to an operation terminal for more than 600 seconds, and removes `rst_*` directories with no row older than 3600 seconds and stale `.readiness-*` files. At most 200 sessions and 200 orphans per sweep, oldest first, never following a symbolic link.
- `ratatoskr-edge` runs it as `spawn_staging_reaper` every `archive_staging.sweep_interval_seconds` (new key, default 300, rule V23, 30..=3600) when an archive receiver is configured, re-evaluates `set_archive_staging_ready` on every sweep (writable and free space at least twice `max_archive_bytes`), and samples three series.
- `open_transfer` replays a token only for an unexpired `open` row (and for a `finalized` row); a `failed` or expired row is replaced by a new session and answers 201.
- `deploy/README.md`, `.env.example`, `deploy/systemd/edge.conf.example` and `deploy/tmpfiles.d/ratatoskr-platform.conf` document the reaper; the false "staging sweep" comment in `finalize.rs` is corrected.
- Deliberate breaks: none on the wire (a reopen after a failed session now returns 201 with a new token where it returned the dead token). A new configuration key with a default. No schema change.

## Capabilities

### Modified Capabilities

- `ai-archive-acceptance`: staging is reaped, readiness is re-evaluated, and a dead session is replaced on reopen.

## Impact

- `crates/public-api/src/archives.rs` and `archives/reaper.rs`, `crates/core/src/config/{model,validate}.rs`, `crates/telemetry/src/metrics.rs`, `services/edge/src/main.rs`, `Cargo.toml` (free-space crate), tests under `crates/public-api/tests`, `crates/core/tests` and `services/edge/tests`, `deploy/README.md`, `deploy/tmpfiles.d/ratatoskr-platform.conf`, `.env.example`, `deploy/systemd/edge.conf.example`.
