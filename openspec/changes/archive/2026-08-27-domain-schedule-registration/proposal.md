## Why

Domain services need to declare the recurring work they own, but the scheduler can only act on rows an operator creates by hand. That leaves completed domain capabilities without a safe, observable way to request their own cadence.

## What Changes

- Add the `cmd.platform.schedule.registration_requested.v1` command contract and a durable, idempotent registration handler.
- Replace operator-authored interval schedules with service-owned named cron schedules; a registration creates, updates, or disables the service/name pair without changing its schedule identity.
- Validate cron expressions and command payloads before storing them; registration never backfills missed occurrences.
- Consume registrations through the existing Edge bus connection and allow only configured producer identities. This is an envelope-level allowlist until the fleet provisions authenticated domain-service bus identities.
- Record registration decisions in the audit trail and expose an operator schedule-status projection with owner, next due time, and latest operation outcome.

## Capabilities

### New Capabilities

- `schedule-registration`: authenticated domain-service registration and operator visibility of recurring schedules.

### Modified Capabilities

- None.

## Impact

- `operations.schedules`, its occurrence model, and the scheduler's deterministic publication calculation.
- Edge command consumption, scheduler configuration, deployment guidance, PostgreSQL grants, and schedule documentation.
- The workspace command contract: producers must publish the documented `cmd.platform.schedule.registration_requested.v1` envelope before the registration consumer is enabled.
