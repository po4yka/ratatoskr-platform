# Ratatoskr Platform

`ratatoskr-platform` provides the public control plane for Ratatoskr Next: identity, authentication, the Edge API, long-running operation tracking, generic ingestion entrypoints, and deterministic scheduling.

> **Status:** milestone 1 of `docs/IMPLEMENTATION_PLAN.md` is implemented. Three binaries (`ratatoskr-edge`, `ratatoskr-ingest`, `ratatoskr-scheduler`) build, load a typed configuration, install telemetry, expose liveness, readiness, metrics and version on an operator listener, and drain on SIGTERM. `ratatoskr-edge` also binds a public listener that currently serves no routes and returns a contract `ErrorEnvelope` on every non-2xx. The database schema, the public API, event handling, idempotency, SSE, capabilities and scheduling described below are planned and are not implemented.

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
platform_ingress.*
```

No schema exists yet; the first migration arrives with milestone 2. The ingress schema is spelled `platform_ingress.*` here and in `AGENTS.md` but `platform_ingest.*` in `docs/ARCHITECTURE.md` S4.1. That contradiction must be settled before the first migration, because a schema name is a migration; it is recorded as open question Q2 in `DEVELOPMENT.md`.

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
- APIs are versioned and generated from the public OpenAPI contract.
- Long work returns `202 Accepted` plus an operation identifier.
- Mutation endpoints require idempotency keys.
- Capabilities are discoverable at runtime so clients do not assume every optional service is deployed.
- Internal service topology is never exposed as a client contract.
- Provider-specific credentials and raw errors are not returned to clients.

A capability response may resemble:

```json
{
  "api_version": "2.0",
  "capabilities": [
    "content.submit",
    "github.catalog",
    "vault.snapshots",
    "telegram.mini_app",
    "social.x",
    "archive.chatgpt",
    "archive.claude"
  ]
}
```

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
2. Add identity, session, and operation schemas with SQLx migrations.
3. Implement NATS outbox/inbox infrastructure.
4. Publish the initial operation and capture APIs.
5. Add SSE operation progress.
6. Implement capability discovery.
7. Add generic device capture ingestion.
8. Add the thin scheduler and integration tests in `ratatoskr-workspace`.

## Workspace integration

`ratatoskr-workspace` pins Platform together with compatible contracts and dependent clients. Platform remains independently buildable and testable. Cross-repository changes involving the public API use an explicit changeset and an expand/migrate/contract rollout.

## Project status

Milestone 1 is implemented: three binaries that configure themselves, report health, expose metrics, and stop cleanly, plus a public listener on Edge whose only behaviour is a correlated contract `ErrorEnvelope`. That is the entire implemented behaviour. Everything else in this README — schemas, endpoints, authentication, idempotency, events, SSE, capabilities and scheduling — documents the intended bounded context and does not exist in the checkout. `DEVELOPMENT.md` states what is present and what is absent, command family by command family.

Three decisions are accepted and binding: [ADR-0002](docs/adr/0002-operation-state-machine-and-progress-semantics.md) (operation state machine, binds milestone 3), [ADR-0003](docs/adr/0003-service-identity-and-producer-name.md) (one wire producer identity for all three binaries; the NATS subject half is deferred to milestone 4), and [ADR-0007](docs/adr/0007-correlation-identity-and-trace-context.md) (correlation identity and trace context).
