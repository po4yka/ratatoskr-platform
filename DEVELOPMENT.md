# Developing Ratatoskr Platform

> Status: Implemented for milestones 1 through 7; milestones 8 through 10 are Proposed.  
> Owner: `ratatoskr-platform`  
> Last reviewed: 2026-08-19

## Current stage

Milestones 1 through 7 of `docs/IMPLEMENTATION_PLAN.md` exist and the commands marked **real** below
are real.

Present: the Cargo workspace, its pinned toolchain and its committed `Cargo.lock`; the
`ratatoskr-platform-core`, `ratatoskr-platform-telemetry` and `ratatoskr-platform-http` library
crates; the `ratatoskr-edge`, `ratatoskr-ingest` and `ratatoskr-scheduler` binaries; typed
configuration with secret-aware values; the internal error type and its public projection; the
`tracing` subscriber with optional OTLP span export; liveness, readiness, Prometheus metrics and
version on an operator listener; SIGTERM draining; and the CI gate in `.github/workflows/ci.yml`.

Present since milestone 2 and 3: the `identity` and `operations` schemas in `migrations/`, a local
PostgreSQL in `compose.yaml`, the `ratatoskr-platform-persistence` pool and embedded migrator, and the
`ratatoskr-platform-identity` and `ratatoskr-platform-operations` crates with their integration suites.

Present since milestone 7: `GET /v2/capabilities`; the `platform_ingest` schema and the generic
webhook adapter at `POST /v2/ingest/webhooks/{source_id}`, served by `ratatoskr-ingest` on a public
listener of its own; and the generated public `OpenAPI` document in `openapi/openapi.json`, written
and drift-checked by `cargo run -p openapic`. `ratatoskr-ingest` now REQUIRES
`RATATOSKR__DATABASE__URL` and a public listener, applies no migrations, and refuses to start against
a database `ratatoskr-edge` has not migrated.

Present since milestone 6: the outbox publisher and the operation-event consumer run inside
`ratatoskr-edge`, and `GET /v2/operations/{id}/events` streams progress as Server-Sent Events with
`Last-Event-ID` replay. The bus is optional — a developer polling `/v2/operations` needs no broker —
but a deployment without one accumulates commands nobody publishes, which the process warns about at
startup, and `GET /v2/capabilities` reports `content.submit` as unavailable in it.

Present since milestone 5: the versioned public API — `POST /v2/captures` and
`GET /v2/operations/{id}` — session authentication, and the idempotency ledger. `ratatoskr-edge` now
REQUIRES `RATATOSKR__DATABASE__URL` and refuses to start without it: every route it serves reads or
writes the database, and a process that started anyway would report itself ready and then fail every
request.

Present since milestone 4: the transactional outbox and the inbox in `operations`, the NATS subject
grammar, a `JetStream` publisher, and the pump that moves claimed rows onto the bus. Both routes that
accept work write their command into the outbox; the pump runs in `ratatoskr-edge` only.

Absent: the OAuth callback facade and Telegram assertion exchange; scheduled command publication —
`ratatoskr-scheduler` still binds nothing but its operator listener; the single-host deployment
profile; the `linux/arm64` artifact; and the workspace end-to-end slice. The OAuth facade and the
Telegram exchange are item 8; the deployment profile is item 9; the artifact and the slice on the
target are item 10. Assigning any of them to a RANGE of milestones assigns them to nobody, which is
how the `Dockerfile` stayed unowned. None of them is scaffolded, stubbed or present in the checkout.

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
| `OpenAPI` | **real** | milestone 7 |
| Artifact | **real** | — |

### Rust — also the CI gate, in this order

```bash
cargo fetch --locked
cargo deny check
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
document is wrong.  That is now asserted by a step in the workflow rather than left to
whoever edits one of the two files.

`cargo deny check` is in the list because nothing else in the gate reads RustSec. When it was first
run against this graph it reported five advisories, four of them a rustls-webpki reachable from
`ratatoskr-edge` and one an unmaintained crate with no safe upgrade — none of which `cargo clippy` or
`cargo test` can see. `deny.toml` also pins the dependency-source policy, including
`required-git-spec = "rev"`, which turns the prose rule in `Cargo.toml` about branches and tags not
pinning into an exit code.

`cargo run -p openapic -- generate` is **not** in the gate, and must never be. It writes
`openapi/openapi.json` from the routes, so a gate that ran it before `cargo test` would regenerate
the artifact it is about to check and pass on any drift at all — the same vacuous sequence the
sibling contracts repository documents. Generating is the FIX, run by a developer who changed a
route; `cargo test` is the CHECK, and it is the only one CI runs.

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

# scheduler, on defaults alone, no environment at all
cargo run --locked -p ratatoskr-scheduler

# ingest needs a database and an explicitly named public bind; it carries no default for one
RATATOSKR__PUBLIC__BIND=127.0.0.1:8181 \
RATATOSKR__DATABASE__URL=postgres://platform:platform@127.0.0.1:5432/platform \
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

`ratatoskr-edge` and `ratatoskr-ingest` each bind a public listener; the edge default is `8080` so it
runs on a DEVELOPER machine with no configuration. `ratatoskr-ingest` has no compiled default and
refuses to start without an explicit bind: on the deployment target `8081` is already held by another
process, and a port on a host with co-tenants is an allocation, not a default. Its allocation there
is `8181` (`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`). `ratatoskr-scheduler` never binds one
(`docs/ARCHITECTURE.md` S18) and refuses to start if one is configured.

A webhook source is registered by an operator — there is no route for it, because who may create a
source on whose behalf is the authorization work of milestone 8:

```sql
insert into platform_ingest.webhook_sources
    (source_id, owner_user_id, label, token_hash, target, created_at)
values (gen_random_uuid(), '<user>', 'an rss shim', digest('<credential>', 'sha256'),
        'content.capture', now());
```

### PostgreSQL — real

```bash
docker compose up -d                 # PostgreSQL 17 on 127.0.0.1:5432, user/password/database `platform`
docker compose down -v               # the documented reset: drops the named volume with the data
```

The major version is pinned to what the deployment target runs, which is **17**
(`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`). ADR-0004 chose runtime-checked queries, so this
container is the only verification the SQL gets, and against a different major it verifies the wrong
thing. Moving the pin is not a tag bump: PostgreSQL refuses to start on a data directory initialised
by another major, so `docker compose down -v` — the documented reset above — is part of the change.

Collation is stated, never inherited. `compose.yaml` and `.github/workflows/ci.yml` set the same
`POSTGRES_INITDB_ARGS`, and `TestDatabase::create` names the locale on every `create database`
instead of copying `template1`. Without that, a text index sorts one way where it is built and
another where it is read — and a `unique` index that does not hold is not a performance problem:
`identities_provider_external_id_key` not holding means one external account maps to two internal
users. ICU rather than glibc because PostgreSQL tracks the ICU version and warns on a mismatch, while
a glibc collation changes silently across a distribution upgrade.

The default `PLATFORM_TEST_DATABASE_URL` matches this compose file, so `docker compose up -d` followed
by `cargo test --workspace` needs no further setup. The integration suite creates a disposable
database per test and drops it afterwards; a test that panics deliberately leaves its database behind
so the failure can be inspected.

`ratatoskr-edge` and `ratatoskr-ingest` both require `RATATOSKR__DATABASE__URL` and refuse to start
without it. Edge owns the migrations and applies them at startup; ingest applies none — S18 gives it a
least-privilege role, and a role that may create a schema is not one — so it checks that the schema is
there and says so in one sentence if it is not.

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

Both streams are created with **stated** limits, never `jetstream::stream::Config::default()`. Every
unset field of that struct is its zero, and under `RetentionPolicy::Limits` those zeros mean "no
limit": the stream retains everything until the store fills and then, under `DiscardPolicy::Old`,
silently deletes the oldest messages — the ones nobody has consumed. The asymmetry between the two
streams is the decision: a command stream **refuses** a publish when full, because the outbox is the
durable copy and a refusal becomes a retry, a bounded backoff and a dead-lettered row an operator can
read; an event stream drops its oldest, because an event is a fact its producer already recorded.

`get_or_create_stream` does not reconcile. A stream created earlier with different limits keeps them,
and the client says nothing — so a deployment carrying corrected limits reports success and changes
nothing. `platform_eventing::stream::ensure` therefore compares and returns the differing fields, and
the services log them at WARN. Fixing one is an operator action against the broker:

```bash
docker exec ratatoskr-platform-nats nats stream rm ratatoskr_commands -f   # then restart the service
```

`PLATFORM_TEST_NATS_URL` overrides the broker the suite uses; the default matches `compose.yaml`.
JetStream is required, not optional: the publisher waits for an acknowledgement, and core NATS
acknowledges nothing.

Subjects are `cmd.<type>` and `evt.<type>` where `<type>` is the contract type name (ADR-0005). The
class prefix is the privilege boundary a NATS credential is granted over.

`ratatoskr-edge` REQUIRES `RATATOSKR__BUS__URL` and refuses to start without it, exactly as it
refuses without a database. It was a warning until milestone 7's survey pointed out what that bought:
edge came up healthy, reported `content.submit` unavailable through `/v2/capabilities`, and piled
every accepted capture into `operations.outbox` with no publisher and no alert — a service that
passes its own readiness check while doing nothing.

`platform_eventing::pump::run_once` is one pass, driven by its caller. `ratatoskr-edge` is the only
caller: `ratatoskr-ingest` writes commands into the outbox and publishes none of them, so a deployment
of ingest without edge accumulates work nobody sends. Where publishers run is a deployment-profile
decision and milestone 9 is where that profile is written.

### Artifact — real

```bash
# The image the deployment runs. `--platform` is not optional: the target is aarch64 and a build that
# omits it produces whatever the builder happens to be.
docker buildx build --platform linux/arm64 \
  --build-arg RATATOSKR_GIT_SHA="$(git rev-parse HEAD)" \
  -t ratatoskr-platform:dev .

# It carries three binaries and no default command, so every caller names one.
docker run --rm ratatoskr-platform:dev ratatoskr-edge check-config
```

`debian:12-slim` is the runtime stage because it IS glibc 2.36, the target's version
(`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`) — an exact match rather than an approximation. A
binary linked against a newer glibc does not start against an older one, and the failure is a loader
error that names nothing useful.

`RATATOSKR_GIT_SHA` is read at COMPILE time through `option_env!`, so a build that omits the argument
produces binaries reporting `git_sha: unknown` through `/version` and `platform_build_info` — the
first thing anyone looks at when a deployment misbehaves. CI passes `github.sha` and asserts the
running container reports it back.

CI builds this on a NATIVE `ubuntu-24.04-arm` runner rather than under QEMU on the x86 gate. This
repository is public, so those runners cost nothing, and emulating a 368-crate Rust build is the
difference between minutes and hours. The job builds and smoke-tests the artifact rather than
re-running the suite: the suite checks behaviour and already does that; this job answers the one
question the suite cannot, which is whether the thing we ship starts on the thing we ship it to.

### `OpenAPI` — real

```bash
cargo run --locked -p openapic -- generate   # write openapi/openapi.json from the route tables
cargo run --locked -p openapic -- check      # exit 1 if it would differ
```

ADR-0006 fixes the direction: Platform generates the document FROM its routes, and
`ratatoskr-contracts` owns the payload types the routes carry — their schemas come from the same
`schemars` derives that produce contracts' published `JSON` Schema, so no type is described twice.

The document cannot drift from the router because there is one table. Each serving crate exposes a
list of `(RouteDoc, MethodRouter)` pairs; `routes()` folds the first half and `surface()` collects the
second. A documented route with no handler does not compile, and a served route with no description
cannot be added without writing one.

`cargo test --workspace` fails when `openapi/openapi.json` is stale, and its message names the command
above. Never edit the file: the routes are the source, and the next `generate` undoes a hand edit.

### `docs/ARCHITECTURE.md` S16 coverage — and the debt in it

The "Emitted" column is the point, and every `no` in it is deliberate. Emitting an absent signal as
an always-zero series would be worse than absence: a panel reading `outbox_lag: 0` and a `for: 5m`
alert that never fires both assert that a component is healthy when nothing is watching it.

Three instruments exist, and only three: `http_server_request_duration_seconds`,
`platform_readiness` and `platform_build_info` (`platform_telemetry::metrics::ALL`, held to exactly
that list by test T-4).

**This table was a plan and has become a debt.** Milestone 1 wrote it with an "Arrives" column, and
milestones 2 through 7 then shipped the SUBJECTS — operations, an outbox, idempotency, SSE, a
capability document — without shipping the instruments that watch them. Every row below whose
milestone has passed is a thing this repository does and cannot see itself do.

| S16 requirement | Emitted | Subject exists since |
|---|---|---|
| request count, latency, status, route template | **yes** — `http_server_request_duration_seconds` and its derived `_count` | milestone 1 |
| dependency health | **yes** — `platform_readiness`, one gauge covering the database probe | milestone 2 |
| capability state | no | milestone 7 |
| authentication and authorization outcomes | no | milestone 5 |
| operation age and transition counts | no | milestone 3 |
| outbox and inbox lag | no | milestone 4 |
| command publication failures | no | milestone 4 |
| idempotency hits and conflicts | no | milestone 5 |
| SSE connection count and delivery lag | no | milestone 6 |
| scheduler drift and duplicate suppression | no | milestone 9, not yet |

Closing this is its own piece of work rather than a tail end of the milestone that happened to notice
it: each row needs a publication point that is truthful about WHEN the value is known, a bounded
label set, and a test that pins the name — a rename silently breaks every dashboard and every alert.
Adding a gauge to tick a row, published from wherever was convenient, would produce exactly the
misleading series the paragraph above refuses.

## Expected workflow

- Run Edge, Ingest, and Scheduler as separate roles from one repository.
- Use typed configuration and secret-aware values.
- Keep public request handlers short; durable work becomes an operation and command.
- Write only `identity` and `operations` owned schemas.
- Add outbox/inbox and idempotency tests with every asynchronous path.
- Generate the public API client from OpenAPI; never hand-maintain duplicate endpoint models.

## Open questions for the repository owner

Each blocks a later milestone or records a contradiction between documents that the milestone which
found it must not resolve unilaterally. Q2 and Q4 are **hard blockers**: they cannot be worked around
in the milestone that hits them. Q2 is closed by milestone 7; Q4 is a one-line change to a sibling
repository and is still open. A struck-through row is answered; it is kept rather than deleted so the
answer stays attached to the question.

| # | Question | Blocks |
|---|---|---|
| Q1 | `README.md` listed `crates/{identity, operations, api-contracts, ingress, platform-infrastructure}`; `docs/ARCHITECTURE.md` S3 lists a different set sharing only `identity` and `operations`. Milestone 1 treats S3 as normative and deleted the README tree. If README's list was intended, S3 must change instead. | milestone 2, which creates the first crate whose name appears in one list and not the other |
| ~~Q2~~ | ~~**Ingress schema spelling.**~~ **Closed by [ADR-0009](docs/adr/0009-one-spelling-for-generic-ingest.md) at milestone 7.** The word is `ingest` wherever it is an identifier: the schema, the crate, the library, the binary, the S18 database role and the `/v2/ingest` path prefix. `README.md` is corrected; `AGENTS.md` turned out to use "ingress" only in prose and needed no change. | — |
| Q3 | **Event family mismatch.** `README.md` names `platform.operation.accepted.v1`, `.completed.v1` and `.failed.v1`, but `ratatoskr-contracts` ships only `platform.operation.progressed.v1`, whose payload is a state-carried `OperationSnapshot` covering every transition. Either Platform emits one event type or contracts gains three. Contracts made the choice; README looks stale. | milestone 4 |
| Q4 | **`correlation` is not in `contracts.toml [entity_kinds].known`.** `EntityKind` is open on the wire, so nothing breaks at milestones 1 through 3, but a Platform event fixture carrying `correlation:` fails `cargo contracts check`. The fix is a one-line contracts changeset. See [ADR-0007](docs/adr/0007-correlation-identity-and-trace-context.md). | milestone 4, **hard** |
| Q5 | Contracts pins `serde_json = "=1.0.151"` and `schemars = "=1.2.2"` exactly. Those are graph-wide constraints: Platform cannot move past them while depending on this contracts commit, including for a security advisory. The remedy is a contracts bump, and it is worth knowing before an advisory rather than during one. | any milestone, on the day it happens |
| Q6 | `docs/ARCHITECTURE.md` S18's "no public listener except health" is read here as "the scheduler binds exactly one listener and it serves only probes", enforced by a startup validation rule. If health was meant to sit on a public listener alongside the API, the readiness body must be narrowed further, because it would then be reachable from the internet. | milestone 1, if the reading is wrong — one validation rule and one router |
| Q7 | The seven-command-families sentence is genuinely ambiguous. This document takes the documentation reading; the literal reading forces a database and a message bus into milestone 1, contradicting `docs/IMPLEMENTATION_PLAN.md` items 2 and 4. | milestone 1, if the literal reading was intended |
| Q8 | `README.md`'s former Observability metric names (`http_request_duration`, `operation_duration`, `outbox_lag`, …) carry no units and mostly have no subject. They are treated as aspirational and were replaced by the three implemented names, with S16 as normative. If those names are a contract with a dashboard that already exists somewhere, that needs saying. | milestone 1 |
| Q9 | `missing_docs`, `unwrap_used`, `expect_used` and `panic` are all denied, inherited from contracts. That is strict for a service: every `LazyLock` constant needs an `#[allow(…, reason = "…")]`. Kept, because a control plane is exactly where an `unwrap` is a 500 — but the `reason =` discipline must hold or the allows become noise. | ongoing |
| ~~Q10~~ | ~~`ingest` and `scheduler` `main.rs` differ by one constant.~~ **Answered at milestone 7, as scheduled: they diverged.** Ingest gained a public listener, a required database, a schema check and a router; scheduler still binds nothing but its operator listener. The duplication is repaid and the two must not be collapsed. | — |

## Rules

Production code may not `unwrap`, `expect` or `panic!`, and every public item is documented; test
bodies may assert with `unwrap` and `expect`. `unsafe_code` is forbidden workspace-wide. Do not add a
crate, a directory or a configuration field for a milestone that has not started.
