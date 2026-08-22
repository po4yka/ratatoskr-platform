# Tasks

## 1. The reaper's behaviour, against a real database

- [x] 1.1 Add the failing test `a_silent_operation_is_failed_with_its_error` in `crates/operations/tests/reconciliation.rs`: insert an operation accepted an hour before the cutoff with no progress entries; run `reconcile::run_once(pool, window, batch, now)`; assert the operation is `failed`, `terminated_at` is set, one error row carries code `platform.operation.stale` with `retryable=true`, the operation row is retryable, and a progress entry for the termination exists. Run it — it must fail because `run_once` does not exist (compile failure of the test binary IS the stated reason here: the module and its API are what task 1.2 adds).
- [x] 1.2 Implement `crates/operations/src/reconcile.rs::run_once` so 1.1 passes: candidate selection bounded and oldest first, per-candidate transaction re-verifying liveness under `FOR UPDATE`, termination through `record_status`, error record and retryable flip in the same transaction. Export the module from `lib.rs`.
- [x] 1.3 Failing test `an_operation_that_reported_inside_the_window_is_never_harvested`: same age on `status_changed_at`, but one progress entry observed inside the window; assert the pass reports zero reconciliations and the operation stays unterminated. Run red (the predicate will be written to use only `status_changed_at` only if 1.2 guessed wrong — if it already passes, the predicate was right and the test documents it; state which happened). **It passed on arrival: 1.2 wrote the `greatest(...)` liveness predicate as designed, so this test documents the behaviour rather than driving it.**
- [x] 1.4 Failing test `a_late_report_after_reconciliation_does_not_resurrect`: reconcile, then apply a `running` report through `ProgressProjection`; assert the outcome is recorded as stale traffic and the operation remains `failed` with its error intact.
- [x] 1.5 Failing tests for idempotence and the batch bound: `two_passes_do_not_double_terminate` (second pass reports zero) and `a_bounded_pass_terminates_at_most_the_batch_oldest_first` (more stale rows than the batch; assert count and that the oldest were chosen).

## 2. Configuration

- [x] 2.1 Add failing cases to `crates/core/tests/config_validation.rs`: below-floor and above-ceiling `RATATOSKR__OPERATIONS__STALE_AFTER_SECONDS` are V19 violations naming the key; zero is refused, not disabled; absent key yields the default 86400. Run red.
- [x] 2.2 Add `OperationsConfig` (`stale_after_seconds`) to `crates/core/src/config/model.rs`, wire it into `PlatformConfig`, add rule V19 to `validate.rs`. Green.

## 3. The metric

- [x] 3.1 Extend T-4 (`crates/telemetry/tests/subscriber.rs`) with `platform_operations_reconciled_total` and add the constant to `metrics::ALL` with its doc comment. The pin breaks until both land; run after both edits.
- [x] 3.2 Increment the counter in `run_once` beside `record_status`'s transition counter, once per terminated operation; extend an existing reconciliation test to assert the counter moved via the counting recorder pattern from `limit.rs`.

## 4. Wiring and documentation

- [x] 4.1 Start the loop in `services/edge/src/main.rs` beside `spawn_retention` (interval 60 s, batch 100 constants). No new failing test: the boot suite already executes this binary as a child process and asserts it starts, serves and stops cleanly.
- [x] 4.2 `.env.example`, `deploy/systemd/edge.conf.example`: document the variable. Documentation task: no failing test.
- [x] 4.3 ADR-0014 `docs/adr/0014-stale-operation-reconciliation.md`: the S14 reading, why not the scheduler path, liveness definition, defaults. Documentation task: no failing test.
- [x] 4.4 `DEVELOPMENT.md`: S16 table row for the new series; correct the "Absent" paragraph (reconciliation leaves the list); note the reaper in the local-run text where the other edge loops are described. `README.md` status paragraph and `AGENTS.md` phase sentence drop stale-operation reconciliation from the absent lists. Documentation tasks: no failing tests.

## 5. Gate

- [x] 5.1 Run the full command list from `DEVELOPMENT.md`. All green.
- [x] 5.2 Archive the change and run `openspec validate --archived`.
