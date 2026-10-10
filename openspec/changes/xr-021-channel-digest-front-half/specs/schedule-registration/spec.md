## ADDED Requirements

### Requirement: Registrations are typed contract payloads from an allowed registrar

Platform SHALL parse a schedule registration command's payload as the contract type `PlatformScheduleRegistrationRequested`, and the shipped configuration SHALL allow `ratatoskr-github` and `ratatoskr-channel-digests` as registrars. A registration whose envelope producer differs from the payload's service name SHALL be rejected.

#### Scenario: channel-digests registers its daily schedule

- **WHEN** `ratatoskr-channel-digests` sends a valid typed registration for service `ratatoskr-channel-digests`
- **THEN** Platform stores one schedule owned by that service

#### Scenario: another producer is rejected

- **WHEN** a producer that is not an allowed registrar sends the same typed registration
- **THEN** the delivery outcome is rejected and nothing is stored

### Requirement: Digest occurrences are published as contract envelopes

Platform SHALL publish a due schedule whose command type is `channel_digest.schedule.occurrence_requested.v1` as a contract `CommandEnvelope` whose command id is the occurrence id and whose payload names the schedule, the occurrence, the due time and the previous grid point (never earlier than seven days before the due time), and SHALL keep the legacy envelope for every other command type.

#### Scenario: a daily digest occurrence

- **WHEN** a schedule with cron `0 6 * * *` and that command type becomes due
- **THEN** the published payload decodes as the contract occurrence type with the prior day's grid point as `previous_due_at`
