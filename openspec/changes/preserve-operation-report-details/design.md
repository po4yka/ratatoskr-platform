## Context

See `proposal.md` for motivation. `OperationReported` already provides typed results, one error, and warnings. The projection currently reduces those types before persistence even though `operation_results` and `operation_errors` already participate in the same inbox transaction.

## Goals / Non-Goals

**Goals:**

- Round-trip the complete existing v1 values through PostgreSQL and `OperationSnapshot`.
- Keep all writes for one report atomic with inbox delivery.
- Preserve existing reconciliation behavior and public authorization.

**Non-Goals:**

- No new event, API version, producer change, migration, result content, or cross-schema access.
- No deployment to or data conversion on the frozen host.

## Decisions

### Store contract values as structured JSONB beside indexed core fields

`BlobRef`, `ErrorEnvelope`, and `WarningEnvelope` are structured types with additive fields. The schema will store their serialized JSON rather than invent a lossy string encoding. Existing diagnostic core columns remain for bounded validation and reconciliation queries; a JSONB envelope retains the complete contract value. This is smaller than normalizing contract-internal fields into Platform-owned columns and survives additive v1 fields.

Alternative: store only code/message/retryable and a string blob identifier. Rejected because it repeats the current loss and cannot round-trip the published type.

### Parse stored JSON through contract types on read

Snapshot construction will deserialize the stored JSON into the pinned contract types. Invalid stored values remain a contract violation rather than being guessed or emitted.

### Keep projection writes inside the existing inbox transaction

The handler receives a PostgreSQL transaction from event delivery. Status, results, and diagnostics will use that transaction before the inbox commit. No external side effect is added.

### Edit `schema.sql` in place

The product is in development and explicitly has no migrations. A fresh PostgreSQL 17 test database proves the new definition.

## Risks / Trade-offs

- [Core columns and JSON could disagree] → Construct both from one typed envelope in one function and test snapshot equality.
- [Older development databases lack new columns] → Recreate them; automatic migration is forbidden.
- [Additive extensions increase row size] → Existing contract and message-size limits remain the bound; no result content is stored.

## Migration Plan

Run tests on a fresh PostgreSQL 17 database, commit on `main`, and push after the full repository gate. Rollback is a commit revert plus database recreation. The host remains frozen.
