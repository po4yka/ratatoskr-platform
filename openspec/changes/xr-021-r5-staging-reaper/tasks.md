## 1. Pin the contracts (R5 preamble)

- [x] 1.1 Move every `ratatoskr-*` git dependency to the round 5 contracts commit, add the direct `ratatoskr-operation-contracts` dependency to `crates/eventing`, refresh `Cargo.lock`, and run the full gate. A dependency pin cannot start from a failing test.

## 2. The sweep (R5-04 rules 2, 3 and 7)

- [x] 2.1 Add `crates/public-api/tests/archive_staging_reaper.rs` (five reasons, an open session survives, orphan age, symlink, 450 candidates in 200/200/50, missing directory, directory before row) and a stub `sweep_once` returning an empty report, and run it RED on the assertions
- [x] 2.2 Implement `sweep_once`, and run 2.1 GREEN

## 3. Configuration, task and readiness (R5-04 rules 1, 4 and 6)

- [x] 3.1 Add the `core` config tests (default 300, env parse, 29 and 3601 refused by V23 naming the key), `archive_staging_watch.rs` (the loop Edge spawns removes an expired fixture within two intervals, readiness follows an unwritable root and a nearly full volume, with an injected free-space probe and a 1 second interval that bypasses V23 only in the test), and run them RED
- [x] 3.2 Add `ArchiveStagingConfig.sweep_interval_seconds` and V23, `archives::staging::run` spawned by Edge next to the observer when a receiver is configured, the periodic readiness check with an injectable free-space probe, the three series, and the `.env.example` and `edge.conf.example` lines, and run 3.1 GREEN

## 4. A dead session is replaced on open (R5-04 rule 5)

- [x] 4.1 Add `ai_archives_reopen.rs` (failed after digest mismatch, open replays, finalized replays and finalize returns the stored completion, expired replaced) and the permanent-refusal case in `ai_archives_receipt.rs` (`ai_archives.rs` is at the line cap) and run them RED on the replayed dead token
- [x] 4.2 Make `open_transfer` read `session_state`, and run 4.1 GREEN

## 5. Documentation

- [x] 5.1 Document the reaper in `deploy/README.md`, say why `tmpfiles.d` keeps age `-`, and correct the false comment in `archives/finalize.rs`. Documentation cannot start from a failing test.

## 6. Gate

- [ ] 6.1 Run the repository gate and `check-config` with the new key set and unset
