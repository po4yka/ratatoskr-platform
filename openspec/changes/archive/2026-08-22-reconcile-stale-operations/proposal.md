## Why

`ARCHITECTURE.md` S14 requires that "stale running operations are reconciled by an explicit
reaper/reconciliation command", ADR-0002 names the reconciler as the third caller of the transition
table and then records that no milestone owns it, and ADR-0010's deployment has no second process to
blame. Today an operation whose worker died — a crashed extractor, a lost progress event, a command
nobody consumed yet — stays `accepted` or `running` forever: `GET /v1/operations/{id}` reports a lie,
its SSE stream never ends, and the only witness is the `platform_operations_oldest_unterminated_age_seconds`
gauge, which DEVELOPMENT.md explicitly calls "the only way to see" the condition. Principle 4 of this
repository says operations are truthful; an operation nobody will ever advance is not.

Cites `ratatoskr-workspace` spec `operation-progress`: Platform owns the complete operation snapshot,
which includes owning its truthful termination when every other writer has gone silent.

## What Changes

- Add a stale-operation reaper to `ratatoskr-edge`, the process that owns the database (ADR-0013):
  one bounded pass per minute, in its own background task beside the retention sweep.
- An operation is stale when it is unterminated and nothing has been observed for
  `RATATOSKR__OPERATIONS__STALE_AFTER_SECONDS`: neither a status change nor a progress entry.
  Liveness is the newest observed fact about the operation, not the status-change time alone, so a
  long-running worker that reports progress every minute is never harvested.
- A stale operation advances to `failed` through the ONE transition applier (`record_status`,
  ADR-0002), gains a safe error record with the stable code `platform.operation.stale`
  (retryable), flips its own retryable flag to match, and appends a user-safe progress message —
  which is what makes the termination visible on SSE and on the next poll, because both read
  persisted state (S5.5).
- New metric `platform_operations_reconciled_total` counting what the reaper terminated; T-4 pins
  the name first.
- Configuration section `[operations]` with validation rule V19; `.env.example` documents it;
  ADR-0014 records why the reaper is an edge task rather than a scheduler-published bus command.

No public API shape changes: clients see only statuses the contract already defines, reached through
the same projection path as any report. The workspace store spec `operation-progress` is unchanged —
Platform consumes reports exactly as before; it merely stops waiting forever for one.

## Capabilities

### New Capabilities

- `operations/reconciliation`: an operation that stops showing signs of life reaches a truthful
  terminal state within a bounded time, through the same transition rule as every other writer,
  without resurrecting afterwards and without touching anything still alive.

### Modified Capabilities

## Impact

- `crates/operations/src/reconcile.rs` (new) — the pass: claim, decide, terminate, count.
- `crates/operations/src/lib.rs` — module export only.
- `crates/core/src/config/` — `OperationsConfig`, default, V19 rule, config tests.
- `crates/telemetry/src/metrics.rs`, `crates/telemetry/tests/subscriber.rs` — new name, pinned.
- `services/edge/src/main.rs` — the reaper loop beside `spawn_retention`.
- `DEVELOPMENT.md` (S16 row, absent-list correction, local-run note), `README.md` status paragraph,
  `AGENTS.md` phase sentence, `docs/adr/0014-stale-operation-reconciliation.md` (new),
  `.env.example`, `deploy/systemd/edge.conf.example`.
- No schema change: every column and constraint the reaper needs exists. No client-visible contract
  change beyond statuses already defined; no workspace changeset required.
