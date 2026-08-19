# Ratatoskr Platform

`ratatoskr-platform` provides the public control plane for Ratatoskr Next: identity, authentication, the Edge API, long-running operation tracking, generic ingestion entrypoints, and deterministic scheduling.

> **Status:** milestones 1 through 8 of `docs/IMPLEMENTATION_PLAN.md` are implemented. Three binaries (`ratatoskr-edge`, `ratatoskr-ingest`, `ratatoskr-scheduler`) build, load a typed configuration, install telemetry, expose liveness, readiness, metrics and version on an operator listener, and drain on SIGTERM. `ratatoskr-edge` also binds a public listener that currently serves no routes and returns a contract `ErrorEnvelope` on every non-2xx. Milestones 2 and 3 add the two schemas Platform owns: `identity` (users, external identity mappings, devices, sessions, rotating refresh tokens, assertions, grants, revocations and the public-action audit trail) and `operations` (operations, attempts, progress history, typed result references and safe diagnostics), together with the operation transition table and a trigger that enforces the same rule for any writer that bypasses it. Milestone 4 adds the transactional outbox and inbox, the `cmd.`/`evt.` subject grammar and a JetStream publisher, so a state change and the message announcing it commit together or not at all. Milestone 5 adds the first public routes — `POST /v2/captures` and `GET /v2/operations/{id}` — session authentication and the idempotency ledger: a capture reserves its key, creates its operation and enqueues its command in one transaction, a retry returns the original operation, and the same key with a different payload is refused. Milestone 6 closes the loop: `ratatoskr-edge` publishes the outbox to `JetStream`, consumes `evt.>` back into the operation projection with inbox deduplication, and streams progress at `GET /v2/operations/{id}/events` — read from persisted state, never from the bus, which is never exposed to a client. Milestone 7 adds `GET /v2/capabilities`, the `platform_ingest` schema and the generic webhook adapter at `POST /v2/ingest/webhooks/{source_id}` — the first thing `ratatoskr-ingest` serves, on a public listener of its own — and the generated public `OpenAPI` document in `openapi/openapi.json`, written from the route tables and drift-checked by the test suite. Milestone 8 adds the two ways a person or a provider gets in: `POST /v2/sessions/telegram` exchanges an assertion from `ratatoskr-telegram` for a session — Platform holds only that service's public key and never the bot token — and the OAuth callback facade relays an authorization code to the service that owns the provider through a one-time record, so the code appears in no command, no log and no redirect. Scheduling described below is planned and is not implemented.

## Role in Ratatoskr Next

Ratatoskr services are intentionally isolated by bounded context. Platform is the stable public boundary through which web, mobile, browser-extension, export-agent, MCP, and other trusted clients interact with the system.

The repository contains three deployable Rust binaries — `ratatoskr-edge`, `ratatoskr-ingest` and `ratatoskr-scheduler` — under `services/`.

The crate layout is documented once, in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) S3, which marks the crates that exist today. A second listing here would be a third layout in a third document, so this README no longer carries one. (The earlier listing here disagreed with S3; recorded as open question Q1 in `DEVELOPMENT.md`.)

### `ratatoskr-edge`

The Edge service owns:

- the public REST API;
- user, device, and client authentication;
- OAuth callback coordination without retaining provider tokens;
- request idempotency;
- operation creation and status projections;
- SSE or WebSocket progress delivery;
- public OpenAPI and capability discovery;
- rate limits, audit context, and safe error envelopes.

It accepts work, validates authority, creates an operation, publishes a command, and returns promptly. It does not run scraping, Git, provider synchronization, or LLM inference inline.

### `ratatoskr-ingest`

The generic ingest process normalizes supported entrypoints such as:

- RSS polling;
- signed webhooks;
- device capture queues;
- import/drop-folder notifications;
- generic URL and file submission.

Provider-specific integrations with their own credentials or interaction state remain independent services. In particular, Telegram Bot API and Mini App behavior belongs to `ratatoskr-telegram`, not to a generic platform adapter.

### `ratatoskr-scheduler`

The scheduler is deliberately thin. It publishes commands according to configured schedules and owns no domain orchestration:

```text
github.sync.requested.v1
x.bookmarks.snapshot_requested.v1
vault.sync.requested.v1
knowledge.reconcile.requested.v1
archive.import_health_check.requested.v1
```

Workers in the owning bounded context decide what work is due and how it should be performed.

## Data ownership

Platform owns its own PostgreSQL schemas, expected to include:

```text
identity.*
operations.*
platform_ingest.*
```

All three exist in `migrations/`. The generic-ingress schema is spelled `platform_ingest`, and so are the crate, the library, the binary, the database role of `docs/ARCHITECTURE.md` S18 and the `/v2/ingest` path prefix: [ADR-0009](docs/adr/0009-one-spelling-for-generic-ingest.md) settled the contradiction this README used to record as open question Q2. "Ingress" survives in prose, where it names the activity rather than an identifier.

Typical records include:

- internal users and linked external identities;
- trusted devices and client registrations;
- short-lived sessions and refresh-token families;
- idempotency records;
- operations and progress events;
- ingress submissions and routing decisions;
- audit records;
- transactional outbox and consumer inbox entries.

Platform never writes directly to GitHub, Vault, Extractor, Knowledge, social, Telegram, or AI-archive schemas. Domain results are referenced using opaque identifiers and projected from service APIs or events.

## Long-running operations

Every user-visible action that may outlive one HTTP request is represented as an operation:

```text
accepted
queued
running
succeeded
partially_succeeded
failed
cancelled
```

An operation may expose:

- a stable `operation_id`;
- current phase and bounded progress;
- user-safe status text;
- structured retryability;
- result references;
- warnings and truthful partial-success data;
- correlation and causation identifiers.

Example request:

```http
POST /v2/captures
Idempotency-Key: 018f...
```

```json
{
  "platform": "instagram",
  "canonical_url": "https://www.instagram.com/reel/...",
  "captured_at": "2026-08-17T10:30:00+04:00",
  "source": "ios_share_extension",
  "note": "Save for later analysis"
}
```

Example response:

```json
{
  "operation_id": "018f...",
  "status": "accepted"
}
```

The corresponding provider service and Knowledge service progress the operation asynchronously through versioned events.

## Commands and events

Platform consumes and emits contracts from `ratatoskr-contracts`. Initial event families include:

```text
platform.operation.accepted.v1
platform.operation.progressed.v1
platform.operation.completed.v1
platform.operation.failed.v1
platform.identity.linked.v1
platform.capture.accepted.v1
```

Platform emits and consumes nothing today; eventing arrives with milestone 4. That list is also stale: `ratatoskr-contracts` ships only `platform.operation.progressed.v1`, whose payload is a state-carried `OperationSnapshot` covering every transition, so `accepted`, `completed` and `failed` have no contract behind them. Either Platform emits one event type or contracts gains three; recorded as open question Q3 in `DEVELOPMENT.md`.

Commands use at-least-once delivery through NATS JetStream. Platform uses transactional outbox/inbox records, globally unique event IDs, and idempotent state transitions. Exactly-once execution is not assumed.

## Authentication model

The public boundary should support:

- browser sessions with secure, HTTP-only refresh cookies;
- device-bound mobile and extension tokens;
- scoped machine credentials for local agents;
- short-lived identity assertions from provider integrations;
- explicit consent records for external write operations;
- revocation and session-family rotation.

Provider access and refresh tokens remain in the provider-owning service. Edge receives only the minimum signed result required to bind an external identity or authorize a workflow.

Telegram Mini App authentication follows the same rule: `ratatoskr-telegram` validates raw Telegram `initData` using the bot secret and returns a short-lived signed identity assertion; Platform creates the Ratatoskr session.

## Public API principles

- Public clients communicate only with Edge.
- APIs are versioned in the path, and the public OpenAPI document is generated FROM the routes (ADR-0006).
- Long work returns `202 Accepted` plus an operation identifier.
- Mutation endpoints require idempotency keys.
- Capabilities are discoverable at runtime so clients do not assume every optional service is deployed.
- Internal service topology is never exposed as a client contract.
- Provider-specific credentials and raw errors are not returned to clients.

`GET /v2/capabilities` answers with the API version, the client-version floors and the capabilities available to the caller:

```json
{
  "api_version": "2.0",
  "minimum_client_versions": { "web": "2.0", "mobile": "2.0" },
  "capabilities": ["content.submit"]
}
```

The array is short because the vocabulary is closed and holds only names this build serves a route for. [ADR-0008](docs/adr/0008-capability-discovery.md) records why: a name on that list is a promise the route tree has to keep, so `github.catalog` and its siblings enter it in the pull request that adds the routes behind them, not before. A capability is reported when the deployment has the components it needs, those components answered their last health probe, and the caller is authorized for it — so `content.submit` disappears from a deployment with no event bus, whose captures would be accepted and never published.

## Security invariants

1. Edge does not retain GitHub, X, Instagram, Threads, ChatGPT, Claude, or Telegram provider secrets.
2. Long-running work is never executed inside request handlers.
3. Every external write requires an authenticated principal and explicit authority.
4. Idempotency state is persisted before commands can be replayed.
5. User-safe errors do not expose secrets, raw provider payloads, or internal topology.
6. All sessions, identity links, consent changes, and privileged operations are auditable.
7. Cross-schema writes are prohibited.
8. Scheduler jobs publish commands but do not import domain repositories or clients.

## Observability

The binaries emit structured `tracing` logs, OpenTelemetry spans, and bounded-cardinality metrics. Three metrics exist today:

```text
http_server_request_duration_seconds{role,method,route,status}
platform_readiness{role}
platform_build_info{role,version,git_sha,rust_version}
```

The signals `docs/ARCHITECTURE.md` S16 requires but that have no subject yet — operation age, outbox lag, idempotency conflicts, SSE delivery lag, scheduler drift, authentication outcomes — are not emitted at all, because an always-zero series asserts that a component is healthy when it does not exist. `DEVELOPMENT.md` carries the full S16 coverage table with the milestone that supplies each one, and the metric naming convention that keeps later milestones consistent.

Correlation IDs connect client requests, operations, commands, domain events, and downstream notifications. Every request is given one server-side, returned in `x-correlation-id`; see [ADR-0007](docs/adr/0007-correlation-identity-and-trace-context.md).

## Non-goals

- Article extraction or browser control.
- LLM prompts, embeddings, or semantic search.
- GitHub synchronization or Git execution.
- Telegram dialogue state and Bot API dispatch.
- Provider OAuth token ownership.
- Direct access to another service's database schema.
- A public multi-tenant SaaS control plane in the initial version.

## Initial milestones

The authoritative sequence is `docs/IMPLEMENTATION_PLAN.md`.

1. Establish the Axum service skeleton and typed configuration. **(implemented)**
2. Add identity, session, and operation schemas with SQLx migrations. **(implemented)**
3. Implement NATS outbox/inbox infrastructure. **(implemented)**
4. Publish the initial operation and capture APIs. **(implemented)**
5. Add SSE operation progress. **(implemented)**
6. Implement capability discovery. **(implemented)**
7. Add generic device capture ingestion. **(implemented)**
8. Add the thin scheduler and integration tests in `ratatoskr-workspace`.

## Workspace integration

`ratatoskr-workspace` pins Platform together with compatible contracts and dependent clients. Platform remains independently buildable and testable. Cross-repository changes involving the public API use an explicit changeset and an expand/migrate/contract rollout.

## Project status

Milestones 1 through 9 of `docs/IMPLEMENTATION_PLAN.md` are implemented. Two of the three binaries serve public routes; `ratatoskr-scheduler` binds only its operator listener and publishes periodic commands from `operations.schedules`. The single-host deployment profile is in `deploy/`. What remains in this README and does not exist in the checkout: rate limiting, registered-device credentials, stale-operation reconciliation, and backup and restore. `DEVELOPMENT.md` states what is present and what is absent, command family by command family.

Every accepted decision is binding: [ADR-0002](docs/adr/0002-operation-state-machine-and-progress-semantics.md) (operation state machine), [ADR-0003](docs/adr/0003-service-identity-and-producer-name.md) (one wire producer identity for all three binaries), [ADR-0004](docs/adr/0004-migration-layout-and-query-checking.md) (migration layout and runtime-checked queries), [ADR-0005](docs/adr/0005-nats-subjects-and-delivery.md) (NATS subjects and delivery), [ADR-0006](docs/adr/0006-public-api-versioning-and-openapi.md) (REST versioning, and who owns the public `OpenAPI` document), [ADR-0007](docs/adr/0007-correlation-identity-and-trace-context.md) (correlation identity and trace context), [ADR-0008](docs/adr/0008-capability-discovery.md) (what a capability is computed from), and [ADR-0009](docs/adr/0009-one-spelling-for-generic-ingest.md) (one spelling for generic ingest), [ADR-0010](docs/adr/0010-single-node-deployment.md) (one process per role, and why the locks stay), [ADR-0011](docs/adr/0011-identity-assertion-trust-model.md) (what an identity assertion is), [ADR-0012](docs/adr/0012-oauth-callback-relay.md) (how an authorization code reaches the service that owns it), and [ADR-0013](docs/adr/0013-single-host-deployment-profile.md) (the single-host deployment profile).
