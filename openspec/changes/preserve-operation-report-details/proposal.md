## Why

Platform currently records the terminal status from a domain `OperationReported` but discards its `BlobRef`, error, and warnings. A successful operation therefore has no usable result pointer, while a failed operation violates the public snapshot invariant and cannot be read truthfully.

## What Changes

- Persist the complete structured `BlobRef` for every reported result.
- Persist the complete typed error and warning envelopes and project them back without dropping additive v1 fields.
- Keep status, result, and diagnostic writes in the existing inbox transaction.
- Add success and failure regression tests shaped like real Extractor reports.
- Edit the current schema definition in place; add no migration and no contract version.

## Capabilities

### New Capabilities

- `operation-report-projection`: Platform durably projects complete v1 domain operation reports into truthful public snapshots.

### Modified Capabilities

None. The fleet-visible requirement is owned by the workspace `operation-progress` capability.

## Impact

Affected code is `crates/operations`, its PostgreSQL tables in `schema.sql`, and projection integration tests. The public v1 response shape and the pinned contracts dependency do not change. Extractor remains compatible because its existing reports already carry the required typed values.

Rollback is a revert of this repository commit and recreation of the development database from the prior `schema.sql`; the frozen host is not changed.
