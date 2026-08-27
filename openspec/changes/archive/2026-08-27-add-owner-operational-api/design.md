## Context

See proposal.md for motivation and the fleet change
`add-operational-status-workspace-integration` for shared behavior. Platform already owns
`identity.grants`, `identity.audit_events`, operations, `operations.schedule_status`, the
RuntimeState readiness facts, and background-sampled gateway capability documents. The shared
operational wire crate is published at Contracts commit
`9a4df8126b495ffc3ad0647441da1690594f25bc`.

## Goals / Non-Goals

**Goals:**

- Enforce owner authorization at the server on every privileged request.
- Reuse Platform-owned projections with bounded keyset queries.
- Produce public status solely from cached, sanitized facts.
- Keep generated OpenAPI tied to the served route table and shared response types.

**Non-Goals:**

- No role table, schema edit, or migration.
- No provider payload, configuration, diagnostic, or topology exposure.
- No backup execution, LLM cost ledger, command palette, localization framework, or new scheduler.
- No request-time fan-out and no operator health endpoint exposure.

## Decisions

### Use the existing live grant as the owner authority

Each admin handler calls one shared authorization adapter backed by the current
`identity.grants` query. Capability discovery uses the same live query and current database
readiness. This makes revocation effective during an existing session and fails closed when the
database cannot answer. A role claim was rejected because it duplicates authority and becomes stale.

### Query owned projections with keyset cursors

Operations use accepted time plus operation id, schedules use next due time plus schedule id, and
audit events use occurrence time plus audit event id. Each query requests one extra row to derive
the opaque next cursor and caps pages at the contract maximum. Offset pagination was rejected
because concurrent inserts can duplicate or skip rows.

### Reuse shared response contracts

List routes serialize `ratatoskr-operational-contracts` page types and operation detail reuses the
existing `OperationSnapshot`. Query adapters select only fields needed by those shapes, so private
payloads cannot enter serialization by accident. Local duplicate response structs were rejected
because they would drift from generated clients.

### Project status from cached observations

The route reads RuntimeState database/bus facts and background gateway observations, maps them to
the four stable public groups, and calls the shared aggregate constructor. It never triggers a
probe. Raw service documents and configured names remain internal. The response always sets
`Cache-Control: no-store`. Fresh dependency calls were rejected because a status endpoint that
waits on the failing dependency cannot report the outage reliably.

### Keep route and OpenAPI registration together

Every new route enters the existing endpoint table with explicit security and shared schema
registration, then `openapic` regenerates the committed document. Hand-editing OpenAPI was rejected
because the repository gate treats it as generated evidence.

## Risks / Trade-offs

- [A database outage prevents both owner checks and operational queries] → Return a dependency
  failure and publish no privileged data; public status remains available from cached readiness.
- [A gateway observation can be old] → Preserve the last successful timestamp and mark stale
  explicitly instead of fabricating availability.
- [Cursor contents expose ordering metadata] → Encode only contracted ordering keys in a bounded
  opaque cursor and reject malformed input.
- [One extra authorization query per admin or capability request] → Keep it bounded to one indexed
  grant lookup; correctness under revocation outranks session caching.

## Migration Plan

1. Contracts is already published at the pinned SHA.
2. Deploy Platform with the new routes and OpenAPI before Web calls them.
3. Provision `platform.owner` out of band using the existing grant mechanism.
4. Deploy Web and run the workspace Compose smoke.

Rollback removes the Platform routes and capability names; existing sessions, grants, and database
data need no conversion because no schema changed. Web must be rolled back first if it depends on
the routes.
