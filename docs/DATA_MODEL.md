# Platform data model

## Owned schemas

`identity.*`: users, sessions, devices, grants, revocations, assertion nonces, and audit context.

`operations.*`: operations, attempts, progress entries, results, safe errors, idempotency records, projections, outbox, inbox, schedules, and schedule occurrences.

Scheduler state contains definitions and dispatch checkpoints only, and it lives in `operations.*` rather than in a schema of its own: a schedule exists to produce an operation and an outbox row, so a fourth schema would make every scheduler transaction a cross-schema write, which the constraint below forbids ([ADR-0013](adr/0013-single-host-deployment-profile.md)). Generic ingress stores routing metadata and blob references, not domain content authority.

## Constraints

- Private root records carry owner/tenant scope.
- Session/device secrets are hashed or encrypted and never emitted in events.
- Idempotency uniqueness includes principal and operation type.
- Terminal operation transitions are immutable except approved annotation.
- Cross-schema foreign keys/writes are forbidden.
- A schema change edits `schema.sql` in place, and it ships with a test that applies the file to a fresh database.

Retention separates security audit, session expiry, operation history, uploaded staging blobs, and idempotency windows.

`ratatoskr-edge` sweeps hourly, in batches, and the split it enforces is mechanical against user-visible. The idempotency ledger and expired OAuth relays go when their own `expires_at` says so, because that expiry is a fact the writer recorded rather than a policy. Processed inbox records, published outbox rows, audit events and schedule occurrences go on configured windows (`RATATOSKR__RETENTION__*`), and three exclusions inside those windows are deliberate: an UNPROCESSED inbox record is a message claimed and never finished, so deleting it would erase the evidence and let the message be applied again; a DEAD-LETTERED outbox row is work a client was told had been accepted and that nobody delivered, so it is kept until a person resolves it; and the inbox window may never be shorter than the event stream's own retention, which startup rule V17 enforces.

**`operations.operations` is swept by nothing.** Operation history is what a user reads, so how long it is kept is a product decision with somebody on the other end of it, and no milestone owns that decision yet. It is named here rather than left to be discovered.
