## Context

See proposal.md. Edge already authenticates registered devices and streams bounded bodies to
configured loopback domain services. Platform owns operation identity, idempotency and public
projection, while provider services own raw archive storage and parsing.

## Goals / Non-Goals

**Goals:**

- Mint one durable, owner-scoped operation before an archive reaches a provider receiver and return
  it to the agent before streaming begins.
- Stream directly from Edge to a configured loopback receiver without Platform storing archive bytes.
- Make the operation id an Edge-minted receiver claim so producer reports join the public snapshot.

**Non-Goals:**

- Provider parsing, BlobStore ownership, archive content inspection, or a Platform table holding
  archive bytes or parser/completeness state.
- New API versions, migrations, provider credentials, or direct client access to internal listeners.

## Decisions

- Add `POST /v1/ai-archives/{provider}` to prepare an operation, and
  `PUT /v1/ai-archives/{provider}/{operation_id}/content` to stream bytes. The preparation route
  validates an exact configured provider/receiver and uses the existing transactionally durable
  idempotency/operation acceptance primitives.
- Commit the operation/idempotency/audit transaction before streaming, then require the agent to use
  the returned operation-bound upload path. This removes the need for a distributed transaction:
  receiver failure can transition the already-visible operation to a safe terminal failure.
- Persist one Edge-owned receipt binding beside the operation: provider token, declared SHA-256 and
  byte size. It contains no archive bytes, filesystem data, receiver response or parser state; it
  is solely the durable authority for the fixed receipt destination and claims injected after the
  client headers are stripped.
- Reuse the existing Gateway transfer budget/client/header sanitizer with an optional Edge-minted
  operation-id extension. The generic gateway remains the only loopback transport implementation.
- Provider configuration belongs in Edge's existing gateway table, with exact `chatgpt` and `claude`
  service names and transfer-class routes. No user-supplied endpoint is accepted.

## Risks / Trade-offs

- [Edge dies after preparation but before receiver acceptance] -> the existing reconciliation window
  reaches a truthful failed terminal result; the agent has already persisted the operation id.
- [A receiver completes storage but its response is lost] -> the operation stays accepted until the
  producer reports progress or reconciliation fails it; duplicate-safe receipt behavior prevents a
  second archive identity.
- [Archive bodies consume memory] -> stream `Body` unchanged and retain the gateway transfer cap;
  no handler reads all bytes.
- [A provider listener is misconfigured] -> startup configuration validation refuses an unsupported
  provider/service pairing before Edge binds.

## Migration Plan

1. Add the backward-compatible Edge preparation/upload routes and receiver claim.
2. Configure loopback ChatGPT and Claude receipt routes with transfer budgets.
3. Update each provider to require the minted operation claim and emit terminal reports.
4. Update the export agent uploader to persist the returned operation id before polling.

Rollback removes the configured archive routes and stops new acceptance. Existing operations retain
their truthful current status; no archive bytes or provider credentials are stored by Platform.
