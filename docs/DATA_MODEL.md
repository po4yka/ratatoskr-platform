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
- Migrations ship with verification, backfill, compatibility, and rollback/forward-fix plans.

Retention separates security audit, session expiry, operation history, uploaded staging blobs, and idempotency windows.
