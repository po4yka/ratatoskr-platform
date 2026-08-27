## Context

See `proposal.md`. Platform persists generic operation result references as JSON and exposes the
same projection to polling clients. Its database grammar must not be narrower than the canonical
`EntityRef` grammar used by the contract.

## Goals / Non-Goals

- Preserve the contract-authored bounded import summary without interpreting archive contents.
- Do not add an archive upload route, parse provider exports, or store provider diagnostics.

## Decisions

- Update the current schema definition's entity-reference checks to the canonical grammar. This
  keeps every stored reference consistent with the typed contract rather than special-casing one
  archive kind.
- Exercise the real PostgreSQL projection with a serialized `OperationReported` event. A unit-only
  serialization test would miss database constraints.

## Risks / Trade-offs

- A broader canonical grammar accepts contract-valid future entity kinds. The result payload remains
  typed and the operation ownership checks remain unchanged.

## Migration Plan

Deploy the schema definition with the normal development bootstrap, then deploy producers after the
contract pin. Rollback stops producer summary emission; older consumers continue to ignore the
optional field.
