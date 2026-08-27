# Owner Operational API Specification

## Purpose

Defines Platform's live owner authorization and bounded, redacted operational inspection surface
for operations, schedules, audit history, and capability discovery.

## Requirements

### Requirement: Every operational route rechecks the live owner grant

Platform SHALL authorize every `/v1/admin/*` request from the current `platform.owner` grant and
SHALL NOT cache that grant in a session or rely on client-side visibility.

#### Scenario: Member is denied without disclosure
- **WHEN** an authenticated member requests any admin operational route
- **THEN** Platform returns forbidden and no operation, schedule, or audit row

#### Scenario: Revocation applies to the next request
- **WHEN** an owner's grant is revoked while its session remains valid
- **THEN** the next admin request is forbidden

#### Scenario: Authorization lookup fails closed
- **WHEN** grant storage cannot answer the owner check
- **THEN** Platform returns a dependency failure without treating the caller as allowed or denied

### Requirement: Capability discovery follows authorization and readiness

Platform SHALL return `platform.operations.inspect`, `platform.schedules.inspect`, and
`platform.audit.inspect` in deterministic order only for a live owner while the operational
database readiness fact is available.

#### Scenario: Owner receives three operational capabilities
- **WHEN** an owner reads capabilities while the database readiness fact is healthy
- **THEN** the response contains all three canonical names in sorted order

#### Scenario: Member and revoked owner receive none
- **WHEN** a member or a revoked owner reads capabilities
- **THEN** none of the operational capability names is present

### Requirement: Owner operation inspection is bounded and truthful

Platform SHALL expose cursor-paginated newest-first operation summaries across users and owner
detail through `/v1/admin/operations`. Filters SHALL be exact and server-side. Summaries SHALL
include only contracted identity, lifecycle, timestamp, and safe failure-code facts.

#### Scenario: Owner paginates operations across users
- **WHEN** operations for more than one user exceed one requested page
- **THEN** the owner receives deterministic rows and a continuation cursor without duplicates

#### Scenario: Failure detail remains redacted
- **WHEN** a failed operation has a safe code and private diagnostic content
- **THEN** the summary exposes the safe code and omits the private diagnostic

#### Scenario: Ordinary ownership semantics remain unchanged
- **WHEN** a member reads another user's operation through the ordinary or admin route
- **THEN** neither route reveals the snapshot

### Requirement: Owner schedule inspection is bounded and payload-free

Platform SHALL expose a deterministic cursor-paginated projection of
`operations.schedule_status` without command payloads, credentials, addresses, or provider
configuration.

#### Scenario: Never-run schedule reports no outcome
- **WHEN** an enabled schedule has no occurrence
- **THEN** its next due time is present and its last outcome is absent

#### Scenario: Disabled failed schedule remains visible
- **WHEN** a schedule is disabled after a failed occurrence
- **THEN** it remains listed with `enabled` false and the recorded failed outcome

### Requirement: Owner audit inspection is bounded and redacted

Platform SHALL expose newest-first cursor-paginated audit rows with a stable identifier tie-breaker
and only the contracted attribution, action, target, outcome, and correlation facts.

#### Scenario: Equal timestamps paginate without gaps
- **WHEN** two audit events share an occurrence timestamp
- **THEN** the event identifier tie-breaker prevents duplication or omission

#### Scenario: System event does not fabricate an actor
- **WHEN** an audit event has no actor user or session
- **THEN** both attribution fields are absent
