# Ratatoskr Platform

`ratatoskr-platform` provides the public control plane for Ratatoskr: identity, authentication, the Edge API, long-running operation tracking, generic ingestion entrypoints, and deterministic scheduling.

> **Status:** every milestone of `docs/IMPLEMENTATION_PLAN.md` is implemented. The three binaries (`ratatoskr-edge`, `ratatoskr-ingest`, `ratatoskr-scheduler`) provide the public capture and operation APIs, generic webhook ingest, operation projection and SSE delivery, periodic command publication, and the single-host deployment profile. Milestone 10 ran the capture, webhook, and scheduled-command slice through the deployed PostgreSQL and JetStream services on the target host. The detailed current inventory and remaining absences are recorded in `DEVELOPMENT.md`.

> [!IMPORTANT]
> **Ratatoskr is in development.** No database holds data that has to survive a schema change.
> While this status holds, these two rules replace what the documents below plan:
>
> - the API and the database keep their first version. There is no `v2` and no later major
>   version.
> - the database has no migrations. One schema definition exists, and a schema change edits it in
>   place.
>
> Only the repository owner changes this status.

## Role in Ratatoskr

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

All three exist in `schema.sql`. The generic-ingress schema is spelled `platform_ingest`, and so are the crate, the library, the binary, the database role of `docs/ARCHITECTURE.md` S18 and the `/v1/ingest` path prefix: [ADR-0009](docs/adr/0009-one-spelling-for-generic-ingest.md) settled the contradiction this README used to record as open question Q2. "Ingress" survives in prose, where it names the activity rather than an identifier.

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
POST /v1/captures
Idempotency-Key: 018f...
```

```json
{
  "url": "https://example.com/article"
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

Platform publishes capture commands and consumes operation reports through contracts from
`ratatoskr-contracts`:

```text
cmd.content.capture.requested.v1
evt.platform.operation.reported.v1
```

Domain services publish `platform.operation.reported.v1`; Platform consumes those reports to update
the public projection. Platform also owns the full-snapshot
`platform.operation.progressed.v1` contract, but does not publish that event today because clients
read the projection through REST and SSE. Open question Q3 in `DEVELOPMENT.md` records this resolved
ownership split.

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

`GET /v1/capabilities` answers with the API version, the client-version floors and the capabilities available to the caller:

```json
{
  "api_version": "1.0",
  "minimum_client_versions": { "web": "1.0", "mobile": "1.0" },
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

The binaries emit structured `tracing` logs, OpenTelemetry spans, and twenty bounded-cardinality
metrics. `platform_telemetry::metrics::ALL` is the canonical implemented-name inventory;
`DEVELOPMENT.md` maps every S16 requirement to its publication point.

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

1. Create the Rust workspace, typed configuration, errors, telemetry, and health endpoints. **(implemented)**
2. Implement the `identity` schema. **(implemented)**
3. Implement the `operations` schema and state machine. **(implemented)**
4. Add the transactional outbox/inbox and NATS identities and subjects. **(implemented)**
5. Implement the authenticated capture API and idempotency. **(implemented)**
6. Project operation reports and expose SSE. **(implemented)**
7. Add capabilities and generic ingest. **(implemented)**
8. Add the OAuth callback facade and Telegram assertion exchange. **(implemented)**
9. Add thin Scheduler publication and the single-host deployment profile. **(implemented)**
10. Build the `linux/arm64` artifact and run the first end-to-end slice on the deployment target. **(implemented)**

## Workspace integration

The planned `ratatoskr-workspace` topology will pin Platform together with compatible contracts and
dependent clients. No workspace repository pins exist yet. Platform remains independently buildable
and testable. Cross-repository changes involving the public API use an explicit changeset.

## Project status

Every milestone of `docs/IMPLEMENTATION_PLAN.md` is implemented, and milestone 10 ran the first end-to-end slice on the deployment target: three systemd units on a Raspberry Pi 5, one command shape from three producers, onto a real `JetStream`. Two of the three binaries serve public routes; `ratatoskr-scheduler` binds only its operator listener and publishes periodic commands from `operations.schedules`. The single-host deployment profile is in `deploy/`. Present since the post-milestone debts sweep: per-actor rate limiting on both public surfaces (a token bucket behind authentication, the contract 429 fault, and its decision counter), and an on-host backup (`deploy/backup/`, daily dump to NVMe and a borg copy to the second volume). Present since ADR-0014: stale-operation reconciliation — `ratatoskr-edge` fails an operation that shows no sign of life for a day with the stable code `platform.operation.stale`. What remains and does not exist in the checkout: registered-device credentials, an off-host backup copy, alert rules, and operation-history retention. `DEVELOPMENT.md` states what is present and what is absent, command family by command family.

Every accepted decision is binding: [ADR-0002](docs/adr/0002-operation-state-machine-and-progress-semantics.md) (operation state machine), [ADR-0003](docs/adr/0003-service-identity-and-producer-name.md) (one wire producer identity for all three binaries), [ADR-0004](docs/adr/0004-migration-layout-and-query-checking.md) (one schema definition and runtime-checked queries), [ADR-0005](docs/adr/0005-nats-subjects-and-delivery.md) (NATS subjects and delivery), [ADR-0006](docs/adr/0006-public-api-versioning-and-openapi.md) (REST versioning, and who owns the public `OpenAPI` document), [ADR-0007](docs/adr/0007-correlation-identity-and-trace-context.md) (correlation identity and trace context), [ADR-0008](docs/adr/0008-capability-discovery.md) (what a capability is computed from), and [ADR-0009](docs/adr/0009-one-spelling-for-generic-ingest.md) (one spelling for generic ingest), [ADR-0010](docs/adr/0010-single-node-deployment.md) (one process per role, and why the locks stay), [ADR-0011](docs/adr/0011-identity-assertion-trust-model.md) (what an identity assertion is), [ADR-0012](docs/adr/0012-oauth-callback-relay.md) (how an authorization code reaches the service that owns it), and [ADR-0013](docs/adr/0013-single-host-deployment-profile.md) (the single-host deployment profile), and [ADR-0014](docs/adr/0014-stale-operation-reconciliation.md) (stale-operation reconciliation).
