# Design: add-operation-cancel-and-list

## Context

The operations schema already anticipates cancellation: `operations.operations.cancellation_requested_at` exists unused, its comment fixes the semantics ("a request, not a state; the operation reaches `cancelled` only when the owning service confirms it stopped"). The rank rule in `platform_operations::transition` makes `cancelled` an advance from every non-terminal source and a duplicate or conflict against terminals, and `record_status` is the single enforced writer. The reaper (`reconcile_one`) demonstrates the transaction shape for contested writes: lock the row `FOR UPDATE`, re-verify the predicate, write through `record_status`. On the HTTP side both router and OpenAPI fold from one route table in `crates/public-api`, refusals are named through `FailureKind` with a single envelope render site, and there is deliberately no 403 — ownership violations render as 404. No query-parameter pagination exists anywhere yet; this change establishes it. See proposal.md for motivation and the two spec deltas for required behaviour.

## Goals / Non-Goals

**Goals:**

- One guarded storage function that classifies a cancellation attempt against current truth under a row lock, reusable by the HTTP handler today and by any future internal caller.
- Exactly one durable cancellation command per operation, enqueued in the same transaction as the request marker.
- A list query whose pagination cannot drift under concurrent inserts and which serves filters from the existing ownership index.
- Route documentation generated from the same table that serves the routes; committed OpenAPI stays byte-identical to the served surface.

**Non-Goals:**

- No new event contract: terminal confirmation keeps arriving on `platform.operation.reported.v1`.
- No consumer implementation in this repository: binding downstream services is their repositories' work.
- No schema change: no migration, no new index unless implementation proves one necessary (none is expected).
- No deletion, no admin cross-tenant queries, no retry-as-resubmit, no changes to attempts history.

## Decisions

### D1. Two-phase cancellation at the public boundary

`POST /v1/operations/{operation_id}/cancel` records `cancellation_requested_at` and enqueues the command; it never writes `status = 'cancelled'` directly.

Why: the schema comment states the invariant, and ADR-0002 truthfulness forbids Platform from claiming a service confirmed stopping when it has not — external actions already taken must not be presumed undone. The direct-transition alternative was rejected precisely because Edge cannot know whether downstream work stopped. Confirmation arrives later on the existing report contract and advances the projection through `record_status` like any other report.

Consequence: between request and confirmation the operation reads as non-terminal with a recorded request; clients learn "stop requested" from the acceptance response, and "stopped" only from the eventual status. If the owning service never responds, the reaper eventually fails the operation with `platform.operation.stale` — truthful, per spec scenario "reconciliation outlives a pending request".

### D2. Classification happens once, under a row lock

New `platform_operations::request_cancellation(executor, operation_id, owner_user_id, now)` returns one of `Requested`, `AlreadyRequested`, `Terminal(Operation)`, `NotFound`. It runs `SELECT ... FOR UPDATE` inside the caller's transaction, checks ownership in the same read, then either updates `cancellation_requested_at` or reports current truth.

Why a lock rather than a conditional `UPDATE ... WHERE`: the caller needs the classification before it can decide whether to enqueue a command, so a second read is unavoidable; taking the row lock also serializes against `record_status` writers and the reaper's `FOR UPDATE`, making the race matrix a property of PostgreSQL row locking instead of application convention. Ownership checked inside the locked read closes the check-then-act gap the singular endpoint tolerates only because it never writes.

### D3. Command subject `cmd.platform.operation.cancel_requested.v1`

Platform-scoped, mirroring the platform-scoped event family (`platform.operation.reported.v1`). Payload carries `{ "requested_at": ... }`; operation identifier, tenant and correlation ride the command envelope fields that already exist. Subject passes the grammar (three segments plus version) and rides the existing `ratatoskr_commands` stream and edge publisher permissions unchanged.

Alternatives rejected: deriving a subject from the operation kind (`cmd.<kind>.cancel_requested.v1`) breaks the four-segment grammar bound for longer kinds and couples Platform's routing to a naming guess about who executes what; per-service subjects would need a routing table Platform does not own. Broadcast cost — consumers see cancellations for operations they do not own — is accepted at this fleet's scale; consumers filter by envelope operation identifier.

### D4. No Idempotency-Key on cancel, by explicit exception

Captures requires the header because replay would otherwise mint two operations. Cancellation's idempotency domain is the operation itself: D2's guarded write makes repeated calls converge on truth and produce exactly one command regardless of headers. Requiring a key would add client ceremony without adding safety. This consciously narrows the README's general mutation principle for this endpoint; the principle text stays untouched because captures and ingest still require keys.

### D5. Status codes: 202 for pending truth, 200 for terminal truth

After the transaction commits, the handler re-reads the operation: non-terminal answers 202 Accepted with the snapshot; terminal answers 200 with the snapshot. Errors are exactly the existing vocabulary: 404 for missing or foreign identifiers (authorization before existence), 401 unauthenticated, 400 malformed path parameter, 429 rate limited by the standard extractor, 504 infrastructure. No new `FailureKind` variant: a cancellation conflict is not an error condition, it is current truth, so the 409 machinery stays reserved for ledger conflicts.

### D6. Keyset cursor over `(accepted_at DESC, operation_id DESC)`

The cursor encodes the last row's `(accepted_at, operation_id)` pair (epoch microseconds plus UUID), base64url-encoded, structurally decoded with strict parsing; undecodable input is a 400. The query walks `WHERE owner = $1 AND (accepted_at, operation_id) < ($cursor) ORDER BY accepted_at DESC, operation_id DESC LIMIT $n + 1`, fetching one extra row to compute the next cursor. Row-tuple comparison matches the composite index `operations_owner_user_id_idx (owner_user_id, accepted_at DESC)` prefix-for-prefix, so pages are index-order walks.

Unsigned cursors are accepted deliberately: a forged but decodable cursor merely moves the continuation window within the caller's own tenant-scoped data, which the cursor of a legitimate page could do anyway. Signing adds key management for zero security gain.

Filters bind as conjunctions before the cursor predicate: exact `status` parsed from the closed vocabulary and exact `kind` validated against the kind grammar; violations are 400s. Page size defaults to 20 and is bounded 1..=100, matching the retired monolith's list bounds. The response object is `{ "operations": [...], "next_cursor": string | absent }`.

### D7. Summary rows are a projection type, not snapshots-per-row

A new API-layer type `OperationSummary` (identifier, kind, status, stage, progress percent, retryability, correlation, the three lifecycle timestamps) maps directly from the list query's columns — no per-row `snapshot()` fan-out to results, errors and warnings. It derives the JSON schema generator's trait and registers beside the existing response schemas. The singular endpoint remains the place heavy payloads load.

### D8. Handlers live beside their sibling, docs in the same table

Both handlers join `read` in `crates/public-api/src/operations.rs` under the existing `operations` tag; two `Endpoint` entries enter the route table so serving and documenting stay compile-enforced pairs. Audit records the accepted cancellation request following the capture handler's audit pattern, with correlation from the request context.

## Risks / Trade-offs

- [Cancel commands broadcast to services that do not own the operation] → envelope names the operation; consumers filter by ownership. Fleet-visible expectation gets recorded in the workspace store when telegram binds, not silently here.
- [Command published while no consumer is bound yet] → the message ages out with the stream's seven-day retention; an unowned pending operation is eventually failed by the reaper with the stable stale code. Truthful at every point.
- [Filtered list pages scan the owner's index partition] → acceptable at personal-scale data volumes; the composite index still bounds every page by ownership first.
- [Request/confirmation gap may feel slow to clients] → the acceptance response carries the live status; SSE and polling surfaces already exist for the transition to terminal `cancelled`.

## Migration Plan

None required: no schema change is planned, the pinned contracts revision does not move, and nothing consumes the new subject yet. Rollback is reverting the commit and restarting; the development database recreates from an unchanged `schema.sql`.

## Open Questions

None blocking. Whether the capabilities vocabulary gains a name for these routes is decided with ADR-0008's rule during implementation: a name enters only in the pull request that adds routes behind it, and if the existing operation routes carry none, these carry none either.
