## Why

Per-actor rate limiting has been enforced since the debts sweep: the token bucket in
`crates/http/src/limit.rs` refuses an actor past its allowance with a contract 429, on edge and on
ingest both. Nothing counts the decision. `AGENTS.md` requires telemetry to cover "rate-limit and
authorization decisions"; authorization has `platform_auth_decisions_total`, but a rate-limit refusal
is invisible until a client complains, and the one question an operator asks — is the limit biting,
and for whom — has no answer on `/metrics`. The status prose in `README.md` and `AGENTS.md` still
lists rate limiting as absent, which misdirects every future reader of either file.

## What Changes

- Add `platform_rate_limit_decisions_total{outcome}` — a counter emitted where the decision is
  made, inside `ActorLimiter::admit`, so every call site (edge's `Principal` extractor, ingest's
  webhook authenticator) is counted by construction rather than by a line somebody remembers.
  `outcome` is a closed set: `admitted` and `refused`.
- Pin the new name in `platform_telemetry::metrics::ALL` (test T-4 breaks first) and add the S16
  row to `DEVELOPMENT.md`.
- Correct the stale status sentences in `README.md` and `AGENTS.md`: rate limiting exists; what
  remains of the backup debt is the off-host copy, not backup itself.

No public API, schema, or configuration changes. The limiter's behaviour — who is admitted, who is
refused, with what response — does not change; only its visibility does.

## Capabilities

### New Capabilities

- `observability/rate-limit-telemetry`: every per-actor allowance decision is counted on the
  metrics surface, at the site where it is decided, with a closed label set.

### Modified Capabilities

## Impact

- `crates/http/src/limit.rs` — the counter, inside `admit`.
- `crates/telemetry/src/metrics.rs` and `crates/telemetry/tests/subscriber.rs` — the new name in
  the documented set T-4 pins.
- `DEVELOPMENT.md` (S16 table row), `README.md` and `AGENTS.md` (status corrections).
- No client-visible behaviour change; no workspace changeset required.
