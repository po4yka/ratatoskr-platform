# ADR-0014: Stale-operation reconciliation

> Status: Accepted
> Date: 2026-08-22
> Milestone: post-plan (unassigned work named in `DEVELOPMENT.md`)

## Context

`ARCHITECTURE.md` S14 requires that "stale running operations are reconciled by an explicit
reaper/reconciliation command". ADR-0002 named the reconciler as the third caller of the transition
table, then recorded that its "at milestone 9" promise was wrong: milestone 9 built the thin
Scheduler and the deployment profile, and the reconciler was left in no milestone. ADR-0010's host
runs one process per role, so there is no second machine and no second instance to blame for an
operation nobody will ever advance.

Until now, an operation whose worker died — a crashed extractor, a lost progress event, a command
published to a subject nothing consumes — stayed `accepted` or `running` forever. `GET
/v1/operations/{id}` reported a status nothing would change, its SSE stream never ended, and the
only witness was the `platform_operations_oldest_unterminated_age_seconds` gauge, which
`DEVELOPMENT.md` explicitly called the only way to see the condition. Public API principle 4 says
operations are truthful; an operation nobody will ever advance is not.

## Drivers

- The transition rule must have exactly one implementation (ADR-0002). A reaper that writes the
  status column directly would be a fourth writer and a second rule.
- A worker that reports progress without ever changing status is ALIVE. `status_changed_at` moves
  only on an applied ADVANCE, so harvesting on status age alone kills healthy long-running work on
  any threshold an operator could plausibly set.
- The host runs one process per role. Any design that assumes a separate reconciler deployment, a
  cron daemon, or a second database writer is designing for a machine that does not exist
  (ADR-0010).
- The schedule machinery refuses to carry it: every schedule occurrence creates an operation owned
  by the schedule's `owner_user_id`, and `schema.sql` records that there is no system principal.
  A reconciliation pass must not pollute a real user's history with a maintenance operation.

## Decisions

### The reaper is an edge background task

`ratatoskr-edge` owns the schema (ADR-0013), already runs the outbox pump, the event consumer, the
observer, the bus prober and the retention sweep as bounded background loops, and is the only
process that may write `operations.operations`. The reaper joins them: one pass per minute, at most
100 terminations per pass, oldest first. This reads S14's "explicit reaper" as the requirement it
is — a named, bounded, observable mechanism — and not as a mandate for a bus round trip.
`ARCHITECTURE.md` is corrected to name the mechanism the deployment actually runs.

### Options considered and rejected

| # | Option | Outcome |
|---|---|---|
| a | Scheduler publishes a reconcile command; edge consumes it | **Rejected.** Every schedule occurrence creates a user-owned operation (`schema.sql` forbids a system principal), so each pass would write maintenance rows into a real user's history; `run_once` offers no operation-less publication, and adding one reshapes the milestone-9 machinery for no benefit. |
| b | A SQL `cron` job inside PostgreSQL | **Rejected.** The termination must go through `record_status` and the audit of what moved must be in this repository's telemetry; a job inside the database is invisible to the process that owns the projection and untestable by the suite. |
| c | Harvest on `status_changed_at` age alone | **Rejected.** A worker reporting progress every minute never advances after its first `running`, so the newest observed fact is the only liveness signal that does not kill healthy work. |
| d | Edge background task over `operations.*` | **Chosen.** |

### Liveness is the newest observed fact

An operation is stale when it is unterminated and
`greatest(status_changed_at, max(operation_progress.observed_at))` is older than the window. The
progress table is indexed for exactly this read (`operation_progress_operation_observed_idx`), and
the aggregate is bounded by the pass's batch.

### One transaction per operation, re-verified under lock

The pass selects candidates without locking, then per candidate opens one transaction that re-checks
the liveness predicate `FOR UPDATE` before writing. A report committed between selection and lock
makes the predicate false and the row is skipped — the worker's fact beats the reaper's arithmetic.
The termination itself goes through `record_status` (ADR-0002's one implementation, which appends
the SSE-visible progress entry and counts the transition metric); the error record
(`platform.operation.stale`, safe message, retryable) and the `retryable = true` flip land in the
same transaction, because `OperationSnapshot::validate` refuses a `failed` operation with no error
(invariant I2) and an operation PLATFORM terminated for silence may honestly be resubmitted.

### The window is one knob, refused at zero

`RATATOSKR__OPERATIONS__STALE_AFTER_SECONDS`, default 86 400 (one day), validated to
`3600..=2_592_000` by rule V19. Zero is refused outright, not read as disabled — the V18 argument
applies verbatim. "Disabled" is spelled by setting the ceiling; the value's meaning is on the value,
not behind a boolean nobody tests. The interval and batch are constants beside their siblings,
because a knob nobody tunes is a knob that lies about being tunable.

A one-day default means scheduled commands for services that do not exist yet sit visibly `accepted`
for up to a month (the ceiling) before the system admits they will not run. That is deliberate: the
alternative fails work that is merely slow to start, and the ceiling is where an operator goes to
say so.

## Consequences

- An unterminated operation reaches a truthful terminal state within `window + interval` of going
  silent, visible on the polling route, on SSE (which reads persisted state, S5.5), and in the
  history a client replays.
- `platform_operations_reconciled_total` answers "what WE moved"; `platform_operation_transitions_total`
  answers "what moved". They stay separate so a misbehaving worker and an aggressive window are
  distinguishable.
- A late report after reconciliation is ordinary stale traffic under the existing rank rule — the
  projection ignores it, and the operation keeps its error record. No new suppression machinery.
- Operations for not-yet-deployed consumers now age into `failed` instead of aging into a lie. The
  first deployment this changes is the one that has a schedule enabled against a service that is not
  running — which is exactly the condition an operator wants surfaced.

## Security and privacy

The error message and progress message are fixed strings from this repository, bounded by the same
column constraints as every user-safe message; no internal topology, provider detail or storage path
is exposed. The reaper reads and writes only `operations.*`.

## Validation

`crates/operations/tests/reconciliation.rs` (R-1 … R-6) against real PostgreSQL: termination with
error record, liveness, no resurrection, idempotence, batch bound, counter. Rule V19 in the config
suite. The boot suite executes `ratatoskr-edge` as a child process with the reaper wired in.
