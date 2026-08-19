# Developing Ratatoskr Platform

> Status: Implemented for milestones 1 through 6; milestones 7 through 10 are Proposed.  
> Owner: `ratatoskr-platform`  
> Last reviewed: 2026-08-18

## Current stage

Milestone 1 of `docs/IMPLEMENTATION_PLAN.md` exists and the commands marked **real** below are real.

Present: the Cargo workspace, its pinned toolchain and its committed `Cargo.lock`; the
`ratatoskr-platform-core`, `ratatoskr-platform-telemetry` and `ratatoskr-platform-http` library
crates; the `ratatoskr-edge`, `ratatoskr-ingest` and `ratatoskr-scheduler` binaries; typed
configuration with secret-aware values; the internal error type and its public projection; the
`tracing` subscriber with optional OTLP span export; liveness, readiness, Prometheus metrics and
version on an operator listener; SIGTERM draining; and the CI gate in `.github/workflows/ci.yml`.

Present since milestone 2 and 3: the `identity` and `operations` schemas in `migrations/`, a local
PostgreSQL in `compose.yaml`, the `ratatoskr-platform-persistence` pool and embedded migrator, and the
`ratatoskr-platform-identity` and `ratatoskr-platform-operations` crates with their integration suites.
No binary opens a pool yet — `database` is an optional configuration section that is validated but not
yet consumed, and the first route that reads persisted data is milestone 5.

Present since milestone 6: the outbox publisher and the operation-event consumer run inside
`ratatoskr-edge`, and `GET /v2/operations/{id}/events` streams progress as Server-Sent Events with
`Last-Event-ID` replay. The bus is optional — a developer polling `/v2/operations` needs no broker —
but a deployment without one accumulates commands nobody publishes, which the process warns about at
startup.

Present since milestone 5: the versioned public API — `POST /v2/captures` and
`GET /v2/operations/{id}` — session authentication, and the idempotency ledger. `ratatoskr-edge` now
REQUIRES `RATATOSKR__DATABASE__URL` and refuses to start without it: every route it serves reads or
writes the database, and a process that started anyway would report itself ready and then fail every
request.

Present since milestone 4: the transactional outbox and the inbox in `operations`, the NATS subject
grammar, a `JetStream` publisher, and the pump that moves claimed rows onto the bus. No service binary
runs the pump yet — the first command is published by the capture API at milestone 5.

Absent:
any versioned public route; OpenAPI in any form; authentication, authorization, idempotency, SSE,
capability discovery, ingress adapters and scheduled command publication; a `Dockerfile` and
deployment profiles. Those are milestones 2 through 10, and none of them is scaffolded, stubbed or
present in the checkout.

## Toolchain

Rust and Tokio, Axum and Tower, figment for configuration, `tracing` with OpenTelemetry, and
Prometheus text exposition. SQLx/PostgreSQL, NATS JetStream and OpenAPI are in the intended stack but
arrive with the milestones named below. The pinned toolchain in `rust-toolchain.toml` is the only
supported one, and every command is run with `--locked` against the committed `Cargo.lock`.

## Command families

The first scaffold pull request must **document** exact Rust, PostgreSQL, NATS, migration, test,
OpenAPI, and local-run commands. It does not make all seven runnable: three of them describe
milestones 2, 4 and 5. Each family below therefore carries a truthful status and the milestone that
makes it real.

| Family | Status | Arrives |
|---|---|---|
| Rust | **real** | — |
| Test | **real** | — |
| Local run | **real** | — |
| PostgreSQL | **real** | milestone 2 |
| Migration | **real** | milestone 2 |
| NATS | **real** | milestone 4 |
| OpenAPI | **does not exist** | milestone 7 |

### Rust — also the CI gate, in this order

```bash
cargo fetch --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --locked
cargo test --workspace --locked
cargo build --workspace --locked --release
```

The debug build is not redundant with the release build at the end, and it is not there for speed.
`services/edge/tests/boot.rs` executes `ratatoskr-ingest` and `ratatoskr-scheduler` as child
processes, and `cargo test` builds the binary of the package under test only — it never produces a
sibling package's plain binary. Skip this step and three of the four boot tests fail on any target
directory that a previous `cargo build` has not already populated, which is every clean checkout.
That is why the failure was invisible on a developer machine and immediate in CI.

This list and the step list in `.github/workflows/ci.yml` are the same list. If they drift, this
document is wrong.

`cargo fetch --locked` resolves the `ratatoskr-contracts` git dependency over `https://`, and
`ratatoskr-contracts` is a public repository, so it needs **no credential at all** — not locally and
not in CI. The workflow holds no secret, and there is no `.cargo/config.toml`: Cargo's built-in git
client reads an `https://` URL without help, which is why the `net.git-fetch-with-cli` workaround
that an `ssh://` URL required is gone.

If `ratatoskr-contracts` is ever made private, this reverses: the four URLs in `Cargo.toml` go back
to `ssh://git@github.com/…`, `.cargo/config.toml` returns with `net.git-fetch-with-cli = true`, and
CI needs a read-only deploy key. That is a **build** credential, not a test credential — and it is
worth not holding one until the day it protects something.

### Test

```bash
cargo test --workspace --locked
cargo test -p ratatoskr-platform-core --locked --test config_validation
```

No test opens a network socket to anything but `127.0.0.1`, starts a container, or reads a
credential. Production credentials are never required for the default tests.

Configuration tests use `figment::Jail`, which gives each test an isolated environment and working
directory. They cannot use `std::env::set_var`: it is `unsafe` in edition 2024 and the workspace
inherits `unsafe_code = "forbid"`.

### Local run

```bash
# edge, with a public listener
RATATOSKR__PUBLIC__BIND=127.0.0.1:8080 \
RATATOSKR__ADMIN__BIND=127.0.0.1:9464 \
RATATOSKR__TELEMETRY__LOG_FORMAT=pretty \
  cargo run --locked -p ratatoskr-edge

# scheduler and ingest, on defaults alone, no environment at all
cargo run --locked -p ratatoskr-scheduler
cargo run --locked -p ratatoskr-ingest

# validate a configuration without starting anything (exit 0 or 78)
cargo run --locked -p ratatoskr-edge -- check-config

# probe it
curl -si localhost:9464/health/live
curl -si localhost:9464/health/ready
curl -s  localhost:9464/metrics | grep '^platform_'
curl -s  localhost:9464/version
curl -si localhost:8080/nope        # 404 + ErrorEnvelope + x-correlation-id
curl -si localhost:8080/health/live # 404 — probes are NOT on the public listener

# watch the drain: readiness flips to 503 while the listener still answers
kill -TERM "$(pgrep -f ratatoskr-edge)"
```

`ratatoskr-edge` binds a public listener that serves no routes; every request to it returns a contract
`ErrorEnvelope`. `ratatoskr-ingest` binds no public listener until milestone 7, and
`ratatoskr-scheduler` never binds one (`docs/ARCHITECTURE.md` S18).

### PostgreSQL — real

```bash
docker compose up -d                 # PostgreSQL 16 on 127.0.0.1:5432, user/password/database `platform`
docker compose down -v               # the documented reset: drops the named volume with the data
```

The default `PLATFORM_TEST_DATABASE_URL` matches this compose file, so `docker compose up -d` followed
by `cargo test --workspace` needs no further setup. The integration suite creates a disposable
database per test and drops it afterwards; a test that panics deliberately leaves its database behind
so the failure can be inspected.

No service binary opens a pool yet. `RATATOSKR__DATABASE__URL` is validated at startup when it is set
(rules V11 and V12) but nothing consumes it: the first route that reads persisted data is milestone 5,
and a pool with no reader would be a connection held open to prove a point.

### Migration — real

```bash
# Migrations are embedded in the binary by `sqlx::migrate!`, so there is no separate apply step in a
# deployment: `Database::migrate` runs them under a PostgreSQL advisory lock, which makes a rolling
# deployment of several replicas safe.
docker exec -i ratatoskr-platform-postgres psql -U platform -d platform < migrations/0001_identity.sql
```

`migrations/` is one flat directory rather than the two `docs/ARCHITECTURE.md` S3 draws, and the
queries are checked at run time rather than by the `sqlx::query!` macros. Both choices, and the
reasons, are ADR-0004.

### NATS — real

```bash
docker compose up -d                 # PostgreSQL and NATS JetStream together
# Edge declares the stream it publishes to (`cmd.>`) and the one it consumes from (`evt.>`).
# JetStream does not acknowledge a publish to a subject no stream covers, so without those streams
# every command would be retried, backed off and eventually dead-lettered.
docker exec ratatoskr-platform-nats wget -q -O- http://127.0.0.1:8222/healthz   # {"status":"ok"}
```

`PLATFORM_TEST_NATS_URL` overrides the broker the suite uses; the default matches `compose.yaml`.
JetStream is required, not optional: the publisher waits for an acknowledgement, and core NATS
acknowledges nothing.

Subjects are `cmd.<type>` and `evt.<type>` where `<type>` is the contract type name (ADR-0005). The
class prefix is the privilege boundary a NATS credential is granted over.

No service binary runs the pump yet. `platform_eventing::pump::run_once` is one pass, driven by its
caller; the first caller is the capture API at milestone 5.

### OpenAPI — does not exist, milestone 7

The routes are the contract until the document exists, which is why they are tested through the real
public pipeline rather than in isolation. ADR-0006 fixes the direction: Platform generates the
document FROM its routes, and `ratatoskr-contracts` owns the payload types the routes carry. It
arrives with the capability endpoint at milestone 7, when there is enough surface for the machinery
to be worth its weight.

### `docs/ARCHITECTURE.md` S16 coverage at milestone 1

The second column is the point. Emitting the absent signals as always-zero series would be worse than
absence: a panel reading `outbox_lag: 0` and a `for: 5m` alert that never fires both assert that a
component is healthy when it does not exist.

| S16 requirement | Milestone 1 | Arrives |
|---|---|---|
| request count, latency, status, route template | **emitted** — `http_server_request_duration_seconds` and its derived `_count` | — |
| dependency health and capability state | **partial** — `platform_readiness`; there is no dependency yet and the two checks that exist are reported honestly | milestone 2 (PostgreSQL), milestone 7 (capabilities) |
| authentication and authorization outcomes | not emitted — no authentication exists | milestones 2 / 5 |
| operation age and transition counts | not emitted — no operations | milestone 3 |
| outbox and inbox lag | not emitted — no outbox | milestone 4 |
| command publication failures | not emitted — no publisher | milestone 4 |
| idempotency hits and conflicts | not emitted — no idempotency | milestone 5 |
| SSE connection count and delivery lag | not emitted — no SSE | milestone 6 |
| scheduler drift and duplicate suppression | not emitted — no schedules | milestone 9 |

## Expected workflow

- Run Edge, Ingest, and Scheduler as separate roles from one repository.
- Use typed configuration and secret-aware values.
- Keep public request handlers short; durable work becomes an operation and command.
- Write only `identity` and `operations` owned schemas.
- Add outbox/inbox and idempotency tests with every asynchronous path.
- Generate the public API client from OpenAPI; never hand-maintain duplicate endpoint models.

## Open questions for the repository owner

None of these blocks milestone 1. Each blocks a later milestone or records a contradiction between
documents that milestone 1 must not resolve unilaterally. Q2 and Q4 are **hard blockers**: they cannot
be worked around in the milestone that hits them.

| # | Question | Blocks |
|---|---|---|
| Q1 | `README.md` listed `crates/{identity, operations, api-contracts, ingress, platform-infrastructure}`; `docs/ARCHITECTURE.md` S3 lists a different set sharing only `identity` and `operations`. Milestone 1 treats S3 as normative and deleted the README tree. If README's list was intended, S3 must change instead. | milestone 2, which creates the first crate whose name appears in one list and not the other |
| Q2 | **Ingress schema spelling.** `README.md` and `AGENTS.md` say `platform_ingress.*`; `docs/ARCHITECTURE.md` S4.1 says `platform_ingest.*`. A schema name is a migration. | milestone 2, **hard** — the first migration fixes it forever |
| Q3 | **Event family mismatch.** `README.md` names `platform.operation.accepted.v1`, `.completed.v1` and `.failed.v1`, but `ratatoskr-contracts` ships only `platform.operation.progressed.v1`, whose payload is a state-carried `OperationSnapshot` covering every transition. Either Platform emits one event type or contracts gains three. Contracts made the choice; README looks stale. | milestone 4 |
| Q4 | **`correlation` is not in `contracts.toml [entity_kinds].known`.** `EntityKind` is open on the wire, so nothing breaks at milestones 1 through 3, but a Platform event fixture carrying `correlation:` fails `cargo contracts check`. The fix is a one-line contracts changeset. See [ADR-0007](docs/adr/0007-correlation-identity-and-trace-context.md). | milestone 4, **hard** |
| Q5 | Contracts pins `serde_json = "=1.0.151"` and `schemars = "=1.2.2"` exactly. Those are graph-wide constraints: Platform cannot move past them while depending on this contracts commit, including for a security advisory. The remedy is a contracts bump, and it is worth knowing before an advisory rather than during one. | any milestone, on the day it happens |
| Q6 | `docs/ARCHITECTURE.md` S18's "no public listener except health" is read here as "the scheduler binds exactly one listener and it serves only probes", enforced by a startup validation rule. If health was meant to sit on a public listener alongside the API, the readiness body must be narrowed further, because it would then be reachable from the internet. | milestone 1, if the reading is wrong — one validation rule and one router |
| Q7 | The seven-command-families sentence is genuinely ambiguous. This document takes the documentation reading; the literal reading forces a database and a message bus into milestone 1, contradicting `docs/IMPLEMENTATION_PLAN.md` items 2 and 4. | milestone 1, if the literal reading was intended |
| Q8 | `README.md`'s former Observability metric names (`http_request_duration`, `operation_duration`, `outbox_lag`, …) carry no units and mostly have no subject. They are treated as aspirational and were replaced by the three implemented names, with S16 as normative. If those names are a contract with a dashboard that already exists somewhere, that needs saying. | milestone 1 |
| Q9 | `missing_docs`, `unwrap_used`, `expect_used` and `panic` are all denied, inherited from contracts. That is strict for a service: every `LazyLock` constant needs an `#[allow(…, reason = "…")]`. Kept, because a control plane is exactly where an `unwrap` is a 500 — but the `reason =` discipline must hold or the allows become noise. | ongoing |
| Q10 | `ingest` and `scheduler` `main.rs` differ by one constant at milestone 1, and `AGENTS.md` forbids collapsing them. Flagged so a reviewer knows it is a deliberate cost, paid once, repaid at milestone 2 when edge and ingest gain a connection pool and scheduler gains a lease. If they have not diverged by milestone 7, that is a signal to collapse them — not to add an abstraction now. | ongoing |

## Rules

Production code may not `unwrap`, `expect` or `panic!`, and every public item is documented; test
bodies may assert with `unwrap` and `expect`. `unsafe_code` is forbidden workspace-wide. Do not add a
crate, a directory or a configuration field for a milestone that has not started.
