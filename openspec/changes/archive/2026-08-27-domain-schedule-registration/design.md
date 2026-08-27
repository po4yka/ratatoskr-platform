## Context

The scheduler currently polls `operations.schedules` populated by operators and advances a fixed interval grid. Domain services need an event-driven registration surface, while ADR-0013 gives Edge the sole existing NATS connection and scheduler remains a database-only publisher.

## Goals / Non-Goals

**Goals:** durable idempotent registration; server-side cron validation and next-occurrence calculation; update-in-place identity continuity; an explicit short-term producer authorization boundary; operator inspection and audit evidence.

**Non-Goals:** a cron UI, interval or duration syntax, a service-specific scheduling policy, catch-up/backfill registration, or a new public API route.

## Decisions

### D1: Edge consumes the registration command; scheduling owns its transactional meaning

`ratatoskr-edge` already has the sole NATS connection and command stream management privilege. It receives a filtered durable consumer for `cmd.platform.schedule.registration_requested.v1` and delegates the handler to `platform_scheduling`; scheduler continues to own only its periodic database pass. Inbox insertion, registration update, audit write, and inbox completion share one transaction.

### D2: Cron replaces the interval grid

Schedules store a five-field UTC cron expression and `next_due_at`. The parser is `cron` 0.17.0, a small Rust runtime dependency whose parser and occurrence iterator are used both at registration and publication. It is needed to validate the accepted language and find calendar-aware next due times; a handwritten parser would be an unsafe public grammar implementation. Its license, advisories, and arm64 build are covered by the existing deny, locked build, and artifact gates.

The first registration computes the first matching instant strictly after receipt. A normal pass computes the next matching instant strictly after the occurrence just handled. When an update arrives while `next_due_at <= now`, it changes future command/template/enablement values but retains the selected due instant; after that occurrence commits, the scheduler uses the stored changed cron. Thus schedule ID plus due time continues to mint the same UUIDv5 occurrence ID across edits.

### D3: One schedule identity per service/name pair

`operations.schedules` holds `service_name`, `name`, `cron_expression`, and the existing command/owner data, with a unique `(service_name, name)` key. Upsert conflict updates mutable fields in place and never replaces `schedule_id`. There are no migrations: `schema.sql` changes in place as required by development status.

### D4: Configure an honest interim authorization boundary

`scheduling.allowed_registrars` is a non-empty bounded list of domain producer names. The handler requires `message.producer == payload.service_name` and membership in the list. This prevents accidental or unauthorised producer names but is not cryptographic authentication: today the deployment profile has no authenticated domain-service NATS identities and Edge alone holds an nkey. Fleet deployment must provision per-service identities permitted to publish this exact subject before treating the envelope producer as authenticated.

### D5: Audit and operator projection are data, not dashboard prose

The handler appends `identity.audit_events` with action, target and decision but no registration payload. `operations.schedule_status` is a view over schedules and each schedule's latest occurrence/operation, giving operators service owner, user owner, next due, enabled state, and last outcome in one bounded query.

## Risks / Trade-offs

- Allowlist-only producer identity can be forged by a principal that already has general command publish permission; fleet identity provisioning is tracked as a deployment limitation.
- Cron timing is UTC-only. This avoids implicit host time-zone and DST semantics; a future timezone contract must be explicit.
- An update may deliberately retain one already-due occurrence. This favors no-miss/no-double continuity over applying a changed template retrospectively.

## Validation

Integration tests cover inbox redelivery, create/update/disable, invalid cron, unauthorized producer, deferred-due edit continuity, and status projection. Unit tests cover cron parsing/next-time behavior. The full `DEVELOPMENT.md` gate runs before integration.
