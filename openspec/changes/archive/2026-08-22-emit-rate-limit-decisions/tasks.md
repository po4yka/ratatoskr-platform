# Tasks

## 1. The counter

- [x] 1.1 Add the failing unit test `a_decision_is_counted_with_its_outcome` in `crates/http/src/limit.rs` (`mod tests`): a hand-rolled recorder inside `metrics::with_local_recorder` must observe `platform_rate_limit_decisions_total` with `outcome="admitted"` after an `admit` that returns `true`, and `outcome="refused"` after one that returns `false`. Run it; it must fail because the counter is not emitted, not because of a compile error.
- [x] 1.2 Emit `platform_rate_limit_decisions_total{outcome}` inside `ActorLimiter::admit` — one increment on every path that returns, `admitted` on the poisoned-mutex path too. The test from 1.1 goes green.

## 2. Registration

- [x] 2.1 Add the failing assertion first: extend `the_metric_name_set_is_exactly_the_documented_set` (T-4, `crates/telemetry/tests/subscriber.rs`) with `platform_rate_limit_decisions_total` and add the constant to `metrics::ALL` in `crates/telemetry/src/metrics.rs` with its doc comment. T-4 fails until both land in the same commit's working tree; the test run after both edits is green. (One pair, split across two files by necessity: the pin and the name.)
- [x] 2.2 Add the S16 row for the new series to the table in `DEVELOPMENT.md` ("per-actor allowance decisions | `platform_rate_limit_decisions_total{outcome}` | the limiter's `admit`, where the decision is made"). Documentation task: no failing test exists for a table row.

## 3. Status corrections

- [x] 3.1 Correct `README.md`'s status paragraph: rate limiting is present (per-actor token bucket on edge and ingest, contract 429); the remaining backup debt is the off-host copy per `deploy/README.md`, not backup itself. Documentation task: no failing test.
- [x] 3.2 Make the same correction in `AGENTS.md`'s current-phase sentence. Documentation task: no failing test.

## 4. Gate

- [x] 4.1 Run the full command list from `DEVELOPMENT.md` (fmt, clippy, nextest against real PostgreSQL and NATS, deny, OpenAPI drift check, OpenAPI generate check, openspec validate). All green.
- [x] 4.2 `openspec validate` the change, archive it, and run the archived validation the CI workflow runs.
