# Proposal: add-operation-cancel-and-list

## Why

The public operation surface today answers only `POST /v1/captures`, `GET /v1/operations/{operation_id}` and its SSE stream. Consumers need more. `ratatoskr-telegram`'s planned capture flow must cancel a pending operation when a user abandons it, and render retry as a brand-new capture rather than a mutation of the old one. The web operational views need to list recent operations with filters instead of guessing identifiers and fetching them one by one. The retired monolith exposed request lists through its API, but its cancellation was internal task management with no user-facing route, so this change defines new public behaviour rather than porting an old contract. The confirmation half of cancellation already exists fleet-wide: an owning service reports `cancelled` through `platform.operation.reported.v1` exactly as the workspace store spec `operation-progress` defines, and Platform's transition machinery advances the projection. What is missing is the request half and the listing surface.

## What Changes

- `POST /v1/operations/{operation_id}/cancel`: session-authenticated, tenant-scoped, semantically idempotent. A call against a non-terminal operation (`accepted`, `queued`, `running`) records the existing `cancellation_requested_at` marker — the schema already describes it as "a request, not a state" — and enqueues one cancellation command through the transactional outbox in the same database transaction, so downstream consumers stop work cooperatively. A call against an already-terminal or already-cancelled operation returns the current truthful snapshot with no write and no command. Cancelling twice yields one command, not two. No Idempotency-Key header is required: the operation identifier is the idempotency domain, and repeated calls converge on current truth by construction.
- `GET /v1/operations`: session-authenticated, tenant-scoped, keyset-cursor pagination over `(accepted_at, operation_id)` with no offset arithmetic and therefore no drift under concurrent inserts, explicit filters for `state` and `kind`, a bounded page size, and a deterministic newest-first order. Each row carries the same projection fields as the singular endpoint minus the heavy payload collections (result references, errors, warnings).
- Both routes enter the single route table so the generated `openapi/openapi.json` documents them from the same source the router serves, keeping the committed-document drift checks green.
- No schema change is expected: `cancellation_requested_at`, the terminal-status rank rule and the ownership index all exist in `schema.sql`. If implementation exposes a gap, the schema definition is edited in place; no migration is added.
- Out of scope: deleting operations, admin cross-tenant queries, mutating attempts history, retry-as-resubmit (clients create new captures instead).

## Capabilities

### New Capabilities

- `operations/cancellation`: requesting cancellation of an owned, non-terminal operation; truthful idempotent answers for terminal ones; cooperative stop via one published command per operation.
- `operations/listing`: paginated, filtered, tenant-scoped enumeration of an owner's operations with a stable cursor and a payload-light summary shape.

### Modified Capabilities

None. The fleet-visible reporting requirement stays owned by the workspace store capability `operation-progress`; this change only consumes it.

## Impact

Affected code: `crates/operations` gains a guarded cancellation-request write and an owner-scoped filtered list query beside the existing storage functions; `crates/public-api` gains two handlers with their route documentation entries and response schema registrations; `openapi/openapi.json` is regenerated. Tests are added first, one failing test per behaviour, in the existing integration suites of both crates. The pinned `ratatoskr-contracts` dependency does not change: the command envelope Platform publishes is already hand-shaped JSON validated by the subject grammar, and the terminal confirmation arrives on the existing event contract. Downstream services bind to the new command subject in their own repositories; nothing in this change breaks them, because today nothing consumes or produces cancellation messages at all.
