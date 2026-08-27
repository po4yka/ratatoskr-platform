# Schedule Registration Specification

## Purpose

Lets an authorized domain service own a named recurring cron schedule while preserving the scheduler's durable, deterministic command delivery guarantees.

## Requirements

### Requirement: Domain services register named cron schedules by command

Platform SHALL consume `cmd.platform.schedule.registration_requested.v1` commands whose payload contains the service identity, a name unique within that service, owner, cron expression, command type, operation kind, object payload template, and enabled state. A valid command SHALL create or update exactly that service/name schedule; a disabled registration SHALL prevent future publication.

#### Scenario: a service registers a nightly reconciliation

- **WHEN** an authorized service sends a valid enabled registration for a new name
- **THEN** Platform stores one schedule owned by that service, computes its first next-due occurrence strictly after registration time, and publishes no backfill

#### Scenario: a registration updates its named schedule

- **WHEN** an authorized service re-registers the same service/name with a changed cron, payload, or enabled state
- **THEN** Platform updates the existing schedule rather than creating another schedule and retains its schedule identity

#### Scenario: a registration disables a schedule

- **WHEN** an authorized service re-registers its name with enabled set to false
- **THEN** the stored schedule is disabled and scheduler passes do not publish further commands for it

### Requirement: Registration rejects unsafe or unauthenticated requests

Platform SHALL reject a registration whose cron cannot be evaluated, whose command data violates the command grammar, whose payload is not an object, or whose producer is not both the named service and a configured allowed registrar. Rejected deliveries SHALL be recorded as rejected rather than retried indefinitely.

#### Scenario: invalid cron is rejected

- **WHEN** a configured service sends a registration with an invalid cron expression
- **THEN** no schedule is stored or changed and the delivery outcome is rejected

#### Scenario: unauthorized producer is rejected

- **WHEN** a producer absent from the configured registrar allowlist sends a registration
- **THEN** no schedule is stored or changed and the delivery outcome is rejected

### Requirement: Schedule edits preserve occurrence continuity

Platform SHALL retain a schedule's identifier when it is updated, and SHALL preserve a due occurrence already selected before the update. The scheduler SHALL derive occurrence identifiers from that retained identifier and the due time, so an edit cannot double-publish an occurrence or discard a due occurrence that was already owed.

#### Scenario: edit after a due occurrence is selected

- **WHEN** a schedule is updated after its prior next-due time is due but before that occurrence is published
- **THEN** the prior due occurrence is published once with its existing deterministic identifier and the changed cron controls only subsequent occurrences

### Requirement: Operators can inspect schedule ownership and outcome

Platform SHALL provide an operator-readable schedule status projection containing each schedule's service owner, user owner, next due time, enabled state, and the latest operation outcome. It SHALL record allowed and denied registration decisions in the audit trail without storing a free-form command payload in that trail.

#### Scenario: status projection reports the latest occurrence outcome

- **WHEN** a schedule has published an occurrence whose operation has reached a terminal status
- **THEN** the operator projection returns that terminal status as the schedule's last outcome together with its owner and next due time
