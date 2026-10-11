## ADDED Requirements

### Requirement: A registration names an active user as its owner

Platform SHALL check `identity.users` inside the registration transaction and SHALL reject, with an audit event and the log class `schedule_owner_unknown`, a registration whose owner does not exist or is not active. A rejected registration SHALL leave no schedule row.

#### Scenario: an unknown owner is rejected

- **WHEN** a registration names an owner absent from `identity.users`
- **THEN** the delivery is rejected, no schedule exists and an audit event is recorded

#### Scenario: a suspended owner is rejected

- **WHEN** a registration names a user whose status is not active
- **THEN** the delivery is rejected, no schedule exists and an audit event is recorded

#### Scenario: an active owner registers

- **WHEN** a registration names an active user
- **THEN** the schedule is stored
