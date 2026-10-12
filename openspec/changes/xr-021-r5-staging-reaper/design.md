## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md R5-04. The contract text is the source of truth; this file records the Platform-side decisions.

## Decisions

- **Library functions, one task.** `sweep_once(pool, root, now, limits) -> SweepReport` holds all logic so it is tested against a real PostgreSQL and a temporary root; `archives::staging::run` is the loop (sweep, readiness, series) and Edge only spawns it, so the real loop is tested with a one second interval that bypasses V23, which bounds the configuration and not the code. `limits` carries the 200-row bounds, the 600 s terminal grace and the 3600 s orphan age so a test can shrink them without weakening V23.
- **Directory first, row second.** A failed `remove_dir_all` leaves the row, so the next sweep retries; a missing directory is success. Candidates are selected oldest first by `opened_at` with `limit 200`, which is also how 450 candidates take sweeps of 200, 200 and 50.
- **Reasons are exclusive and ordered.** `failed`, then `finalized`, then `expired`, then `terminal`, so one row counts under exactly one reason. The contract reaps a `finalized` row because its directory has no further use, so a `finalized` row is removed at the next sweep, directory present or not; a retried `finalize` returns the stored completion only until then.
- **No symlink traversal.** Entries are read with `symlink_metadata`; only directories whose name matches `rst_[0-9a-v]{24,64}` and files named `.readiness-*` are candidates. A symlink named like a token is ignored, so the sweep cannot leave the root.
- **Readiness is re-evaluated.** `staging_is_writable` runs every sweep, together with a free-space probe (`fs4::available_space`) that must report at least `2 * max_archive_bytes`. The probe is injected so a test does not need a full volume.
- **`open_transfer` reads `session_state`.** `open` and unexpired replays the token; `finalized` replays it; `failed` or expired removes the old directory and row and inserts a new session.
- **tmpfiles.d keeps age `-`.** The reaper owns removal, because systemd age cleanup would delete a directory of a session that is still being resumed.
- **The promised sweep.** `archives/finalize.rs::end_delivery` and the archived change `xr-021-ai-archive-upload-path` mention a staging sweep that did not exist until this change. History is not rewritten; the comment in `finalize.rs` is corrected.

## Risks / Trade-offs

- A reaped `finalized` row makes a late `finalize` retry answer 404 and lets `open` mint a new session for an operation that was already delivered; the operation itself is unaffected, and the receiver sees the same digest again if a client uploads it a second time.
- The free-space check makes readiness flap on a nearly full volume; the staging flag already gates `prepare`, so a flap refuses new archives and nothing else.
