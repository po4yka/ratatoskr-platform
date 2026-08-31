## Context

See proposal.md. Platform currently prepares an operation but then directly proxies one whole-file
request to a provider. That route cannot resume after restart and the still-active
`accept-ai-archive-operations` design explicitly makes Platform staging a non-goal. The deployed
gateway also omits both archive receivers, and the single operation-report consumer has neither
provider-scoped ingress nor an operation/provider binding check.

This design supersedes the earlier direct-proxy and non-staging decisions for AI archives. XPA-020
replaces that development path in place; it does not keep a compatibility route.

## Goals / Non-Goals

**Goals:**

- Preserve one operation across interruption, Edge restart, transfer expiry and idempotent replay.
- Keep authorization and provider routing bound to live Platform device authority.
- Verify bytes before a fixed provider receipt and durably consume provider terminal reports.
- Make route refusal and existing admin readiness reflect every dependency needed for completion.

**Non-Goals:**

- A second API or schema version, database migrations, provider credentials in repository files, or
  a public archive capability token.
- Provider archive parsing, raw archive ownership or terminal-summary calculation.
- A direct client-to-provider session or a parallel whole-file fallback.

## Decisions

### Platform owns operation-bound staging

The current schema is edited in place to bind the acceptance record to the preparing device and
immutable blob declaration, and to store one active transfer generation plus acknowledged chunks.
Chunk content lives under a configured private staging root. The API uses the exact existing
`ratatoskr-blob-transfer-contracts` input pinned at the fleet Contracts revision.

An expired transfer may create a replacement generation only beneath the same operation and
declaration. Database uniqueness and conditional writes make prepare/open/chunk replay deterministic.
Valid foreign owner, device or provider access is deliberately collapsed to 404; a revoked or
otherwise invalid credential never reaches the route and receives the common authentication error.

### Chunk publication and final assembly are crash-safe

A chunk is streamed under hard size limits to a same-filesystem temporary file, synced and renamed
before its acknowledgement is committed. Replays reconcile an already-published file with its
recorded digest. Finalization opens chunks in order and streams them through size and SHA-256
verification into a temporary assembled file. Only verified bytes are reopened for the fixed
provider receipt. A mismatch makes no network call; an uncertain provider outcome remains
reconcilable and does not silently create a new operation.

### Reports have two fixed ingress subjects and one unchanged envelope

ChatGPT publishes the existing `platform.operation.reported.v1` EventEnvelope on
`evt.ai-archive.chatgpt.operation.reported.v1`; Claude uses the corresponding Claude subject.
Platform owns fixed durable consumers for both. Projection receives the ingress binding and rejects
an envelope unless subject, stable producer identity and the provider recorded on the archive
operation agree. This closes the gap left by broker-only authorization.

Deployment NATS configuration declares separate least-privilege users and file-based credential
settings. It never embeds generated credentials or permits anonymous fallback.

### Readiness stays private and route-specific

Staging health and each provider's receiver/report-consumer health are registered in the existing
admin readiness projection. Prepare consults the same provider-specific state and returns bounded
not-found when that provider cannot complete work. The public capability document is unchanged.

## Risks / Trade-offs

- [A crash can leave a published chunk without a database row] -> Reconcile deterministic paths and
  digests on replay; expiry cleanup treats unreferenced files as bounded orphans.
- [Staging increases Edge disk responsibility] -> Bound archive size, chunk size/count, concurrent
  sessions, lifetime and retention; keep the production root on the durable NVMe mount.
- [Provider response loss makes delivery outcome uncertain] -> Reuse operation/digest idempotency
  at the receipt boundary and preserve the sealed staging material until reconciliation.
- [Two consumers can diverge operationally] -> Give each a fixed durable identity and independent
  readiness check so one provider can fail closed without hiding the other.

## Migration Plan

1. Deploy the current schema definition, private staging directory and Edge configuration together
   on a fresh development database.
2. Start both provider receivers and their credentialed report publishers.
3. Start Edge, require both fixed durable consumers, and enable preparation only for healthy paths.
4. Roll back by refusing new preparation first, draining in-flight work, then restoring the prior
   runtime. Preserve operation rows and staged/provider archives for diagnosis.
