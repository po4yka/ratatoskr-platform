# Design

## Why an edge task and not a scheduler-published command

S14's sentence says "reaper/reconciliation command", which reads today as a scheduler schedule whose
occurrence becomes a command that edge consumes. The machinery refuses that shape: every schedule
occurrence creates an operation owned by the schedule's `owner_user_id` (`schema.sql` — "There is no
system principal"), so each reconciliation pass would pollute a real user's history with a maintenance
operation, and `run_once` offers no operation-less publication. Reconciliation is platform-internal
maintenance over platform-owned data — the same class as the retention sweep, the observer gauges and
the database prober, all of them edge background loops for exactly the reason ADR-0013 records: edge
is the process that owns the schema. The reaper joins them. ADR-0014 records this reading of S14 and
ARCHITECTURE.md is corrected to name the mechanism the deployment actually runs.

## Liveness is the newest observed fact

`status_changed_at` moves only on an applied ADVANCE. A worker that reports `running` with progress
percentages every minute never advances after the first report, so harvesting on status age alone
would kill healthy long-running work on a threshold any operator could plausibly set. Liveness is
therefore `greatest(status_changed_at, max(operation_progress.observed_at))`. The cost is one
aggregate subquery over the progress table per candidate row — bounded by the batch, indexed by
`operation_progress_operation_observed_idx`.

## One transaction per operation, re-verified under lock

The pass selects candidate identifiers without locking (bounded, oldest first), then opens one
transaction per candidate: re-select the row `FOR UPDATE` with the same liveness predicate, and only
if it still qualifies apply the termination. This closes the race where a worker reports between
selection and termination — inside the transaction the fresh progress entry makes the predicate
false and the row is skipped. It is the same shape the scheduler uses ("one transaction per
schedule"), for the same reason.

The termination itself goes through `record_status` — ADR-0002's one implementation of the rule,
which also appends the SSE-visible progress entry and counts the transition metric. The error record
(`record_diagnostic`, severity `error`, code `platform.operation.stale`, safe message, retryable) and
the `retryable = true` flip land in the same transaction, because `OperationSnapshot::validate`
refuses a `failed` operation with no error (invariant I2), and an operation the PLATFORM terminated
for silence may honestly be resubmitted.

The database trigger `operations_guard_status_transition` permits every advance the reaper can
attempt (`accepted|queued|running → failed` rises in rank); a concurrent writer that won the race
makes `apply` return Duplicate or Stale and the pass counts nothing for that row.

## Defaults and bounds

Window default 86 400 s (24 h): longer than any plausible broker outage plus drain-plus-restart on
this host, shorter than forever; validation V19 accepts 3600..=2592000 (1 h..30 d) and refuses zero
outright — the V18 argument applies verbatim, and "disabled" is spelled by setting the ceiling, not
by a boolean nobody tests. Interval 60 s and batch 100 are constants beside their siblings
(`PUMP_INTERVAL`, `RETENTION_BATCH`): knobs nobody tunes are knobs that lie about being tunable.
A 30-day ceiling leaves scheduled commands for services that do not exist yet visibly accepted for
a month before the system admits they will not run — deliberate: the alternative fails work that is
merely slow to start.

## Metric

`platform_operations_reconciled_total` increments once per operation actually terminated, beside
`record_status`'s transition counter rather than instead of it: transitions answer "what moved",
this answers "what WE moved", and conflating them makes a misbehaving worker indistinguishable from
an aggressive window. T-4 pins the name first.

## Tests

Integration tests live in `crates/operations/tests/reconciliation.rs` against real PostgreSQL —
every claim worth making here is about a predicate, a lock or a constraint, none of which a unit
test observes. Config rule V19 gets a case in the existing config-validation suite. The metric name
breaks T-4 first, as designed.
