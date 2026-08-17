# Ratatoskr Platform Agent Instructions

## Scope

These instructions apply to the `ratatoskr-platform` repository and its planned deployables:

- `ratatoskr-edge`;
- `ratatoskr-ingest`;
- `ratatoskr-scheduler`.

Repository-local instructions in deeper directories may add stricter rules for a specific binary, but they must not weaken the boundaries defined here.

## Repository mission

`ratatoskr-platform` is the public control plane for Ratatoskr. It owns:

- internal user identity and registered devices;
- authentication and public sessions;
- public REST/OpenAPI surfaces;
- idempotent command acceptance;
- operation lifecycle and progress projections;
- generic ingress normalization;
- thin periodic scheduling;
- capability discovery for clients;
- audit context and request-level policy enforcement.

It accepts work, routes it to the owning bounded context, and reports progress. It must not become the new universal backend that performs every operation inline.

## Current phase

The repository is in architecture bootstrap. Do not assume Rust crates, binaries, migrations, API routes, NATS streams, or CI commands exist unless they are present in the checkout.

When creating initial scaffolding:

- keep `edge`, `ingest`, and `scheduler` separable as binaries;
- share only narrow platform primitives;
- avoid a global application service that imports every domain client;
- document implemented behavior separately from planned architecture.

## Sources of truth

Use this order:

1. active task/changeset and accepted ADRs;
2. `README.md`;
3. public and internal contracts from `ratatoskr-contracts`;
4. repository code and migrations;
5. client assumptions only after they are confirmed by the public contract.

Do not change public behavior solely to satisfy an undocumented client dependency. Update the contract and affected clients through a coordinated changeset.

## Hard bounded-context rules

### Platform owns

- `identity.*` data;
- `operations.*` data;
- session, device, and API token metadata;
- request idempotency records;
- public API authorization and policy decisions;
- operation status, progress, and user-facing error projections;
- generic capability and health metadata;
- scheduler trigger state.

### Platform does not own

- extracted documents or article bodies;
- LLM analyses, embeddings, or search indices;
- GitHub stars, repository metadata, or GitHub credentials;
- Git mirrors, bundles, snapshots, or restore manifests;
- X, Instagram, Threads, ChatGPT, Claude, or Telegram provider credentials;
- provider-specific bookmark/archive state;
- Telegram dialogue, callback, webhook-update, or Mini App interaction state;
- service-private database tables.

Never solve a missing integration by reading or writing another service's schema directly.

## Public API principles

1. **Clients call Platform, not internal services.** Web, mobile, extensions, Mini Apps, and local agents use the public Edge API.
2. **Long-running work is asynchronous.** Accept the request, create an `operation_id`, publish a command, and return promptly.
3. **Every retriable write is idempotent.** Require or derive a stable idempotency key and persist the result.
4. **Operations are truthful.** Distinguish accepted, queued, running, partially completed, completed, failed, and cancelled states.
5. **Errors are stable and actionable.** Return contract error codes; keep provider diagnostics in authorized internal records.
6. **Capabilities replace frontend assumptions.** Expose supported features and minimum compatible client versions.
7. **Pagination, filtering, and ordering are explicit.** Do not expose unbounded list endpoints.
8. **Public contracts are versioned.** Breaking changes require a new API/contract version and coordinated migration.
9. **Authorization is resource-based.** Never rely only on route-level authentication when the object has an owner.
10. **No provider secrets cross the public boundary.** OAuth tokens remain in provider services.

## Operation model

An operation is the Platform projection of work performed elsewhere. It should retain:

- operation ID;
- internal user ID;
- request type;
- correlation ID;
- idempotency key;
- current state;
- monotonic or ordered progress sequence;
- user-safe status message;
- stable error code;
- result references;
- creation/update/completion timestamps;
- audit context.

Operation updates may be delivered at least once and out of order. Projection logic must be idempotent and reject stale regressions.

Do not store full private domain payloads in operation rows when a stable result reference is sufficient.

## Command and event handling

- Use contracts from `ratatoskr-contracts`.
- Publish commands through a transactional outbox.
- Process result/progress events with an inbox or equivalent deduplication mechanism.
- Preserve correlation and causation IDs.
- Assume at-least-once delivery.
- Make state transitions repeatable.
- Separate transient infrastructure failures from permanent validation/policy failures.
- Send exhausted work to a diagnosable dead-letter path rather than silently dropping it.

Do not promise exactly-once execution at the public API layer.

## `ratatoskr-edge` rules

Edge may:

- authenticate users and devices;
- validate public requests;
- enforce rate, size, ownership, and capability policies;
- create operations and commands;
- serve authorized projections;
- stream operation progress through SSE/WebSocket;
- handle OAuth callback routing without retaining provider tokens;
- expose OpenAPI, capabilities, health, and version metadata.

Edge must not:

- scrape URLs;
- launch Chromium;
- execute Git;
- call LLMs for domain work;
- synchronize provider accounts;
- parse ChatGPT/Claude exports;
- wait synchronously for long workers;
- fan out to every service on each page request;
- join private service schemas as a runtime read model.

When a cross-domain view is required, create an explicit projection/query contract with clear ownership and staleness semantics.

## `ratatoskr-ingest` rules

Ingest is a thin entry adapter for generic sources such as RSS, standard webhooks, or normalized drop signals. It should:

- authenticate the ingress source;
- validate size/type limits;
- normalize the input into a stable command;
- assign idempotency/correlation metadata;
- store only the ingress state it owns;
- return or acknowledge quickly.

It must not perform article extraction, summarization, social synchronization, or GitHub backup work.

Telegram Bot API updates, callback/dialogue state, Mini App validation, and Telegram message projections belong in `ratatoskr-telegram`, not in generic Platform ingress.

## `ratatoskr-scheduler` rules

Scheduler only decides **when to request work**. It may publish commands such as:

```text
github.sync.requested.v1
x.bookmarks.snapshot_requested.v1
vault.sync.requested.v1
knowledge.reconcile.requested.v1
```

Scheduler must not:

- import provider SDKs;
- query provider APIs;
- execute domain use cases;
- own domain retry state;
- mutate service tables;
- become a general background worker.

The receiving service owns execution, concurrency, retries, and provider-specific scheduling constraints.

## Authentication and identity

- Internal users use Ratatoskr UUIDs, not Telegram IDs, GitHub IDs, email addresses, or provider account IDs as primary identity.
- Provider identities are linked through explicit verified mappings.
- Device credentials are scoped, revocable, rotated, and stored using platform-appropriate secure storage on clients.
- Sessions are short-lived where practical and refresh/revocation behavior is explicit.
- OAuth `state`, PKCE, nonce, callback-user binding, and scope checks are mandatory where applicable.
- The Platform may coordinate OAuth callbacks but must transfer credentials only to the owning provider service through a protected internal flow.

Never log bearer tokens, authorization codes, cookies, raw Mini App `initData`, or secret headers.

## Data access and migrations

- Platform writes only its owned schemas.
- Cross-schema foreign keys and cross-schema writes are forbidden.
- Migrations are forward-safe and independently deployable.
- Public request acceptance must remain compatible during rolling deployment.
- Destructive schema contraction follows expand/migrate/contract.
- Audit/event records required for security or idempotency are not deleted as incidental cleanup.

If a feature requires a new service-owned field, change that service contract instead of adding a shadow copy to Platform without an explicit projection design.

## Security requirements

- Apply request body, file, decompression, and rate limits before expensive processing.
- Validate callback and webhook authenticity before creating operations.
- Use constant-time comparison where secret validation requires it.
- Do not expose internal topology, provider raw errors, stack traces, or storage paths to clients.
- Treat URLs and uploaded archives as untrusted references; route them to the owning safe processor.
- Enforce object ownership on reads, progress streams, retries, and cancellation.
- Record security-relevant external writes and consent decisions in the audit trail.
- Keep admin and diagnostic endpoints separate from the public user surface.

## Observability

Every accepted request and published command should carry:

- request ID;
- correlation ID;
- authenticated user/device context in a non-sensitive form;
- operation ID when applicable;
- trace context.

Required telemetry should cover:

- API latency and errors;
- command acceptance and outbox lag;
- operation age and terminal state;
- inbox duplicates and stale progress events;
- scheduler trigger lag;
- rate-limit and authorization decisions without leaking sensitive data.

Logs are diagnostic evidence, not the source of operation state.

## Testing expectations

When implementation exists, use the applicable checks:

- route and middleware unit tests;
- authorization/ownership matrix tests;
- idempotency replay tests;
- operation state-machine tests, including out-of-order events;
- contract and OpenAPI compatibility tests;
- outbox/inbox integration tests with PostgreSQL and NATS;
- webhook/OAuth callback validation tests;
- scheduler tests proving it only emits commands;
- rate/size limit tests;
- end-to-end tests through the workspace for representative workflows.

Never use real provider credentials or personal production data in fixtures.

## Cross-repository change rules

A Platform change requires a workspace changeset when it affects:

- public API contracts or generated clients;
- event/command contracts;
- authentication flows used by another client/service;
- operation result semantics;
- deployment order;
- capabilities exposed to web/mobile/Telegram Mini App/extensions.

The changeset must list producers, consumers, compatible rollout order, and rollback behavior.

## Git and PR workflow

- Keep Edge, Ingest, and Scheduler changes separated by concern even when they share one repository.
- Avoid combining public API changes with unrelated infrastructure refactors.
- Document all new endpoints, permissions, commands, and operation states.
- State whether the change is backward compatible.
- Include migration and rollout notes for persistence or contract changes.
- Do not merge a producer-only breaking contract change before consumers can handle it.
- Do not commit secrets, local endpoints, or temporary cross-repository path overrides.

## Completion criteria

A task is complete only when:

- responsibility clearly belongs to Platform;
- no provider/domain implementation leaked into the control plane;
- the public and event contracts are explicit and compatible;
- idempotency and operation-state behavior are defined;
- ownership and authorization tests cover the new surface;
- outbox/inbox and retry semantics remain safe;
- migrations are independently deployable;
- telemetry contains correlation without secrets;
- repository-local checks pass;
- affected clients and services are validated through the workspace changeset.
