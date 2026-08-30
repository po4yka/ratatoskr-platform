## Purpose

Define Platform's fixed GitHub JetStream consumers and least-privilege application identity.

## ADDED Requirements

### Requirement: Platform provisions four exact GitHub durables

Platform SHALL provision `ratatoskr_github_sync` on `ratatoskr_commands` filtered to `cmd.github.sync.requested.v1`, and `ratatoskr_github_analysis_completed`, `ratatoskr_github_analysis_failed`, and `ratatoskr_github_vault_policy_ack` on `ratatoskr_events` filtered respectively to `evt.knowledge.repository_analysis.completed.v1`, `evt.knowledge.repository_analysis.failed.v1`, and `evt.vault.backup_policy.acknowledged.v1`. Each SHALL use explicit acknowledgement, deliver-all replay, finite acknowledgement wait, finite delivery attempts, and idempotent exact-config validation.

#### Scenario: Existing exact consumers are reused
- **WHEN** provisioning runs repeatedly against the required four consumers
- **THEN** their configuration and cursors remain unchanged

#### Scenario: Drift is refused
- **WHEN** a named consumer exists with a different stream, filter, acknowledgement, replay, wait, or delivery limit
- **THEN** Platform reports the mismatch and does not mutate or replace it

### Requirement: GitHub receives only declared message authority

The GitHub NKey SHALL publish only `evt.knowledge.repository_analysis.requested.v1` and `cmd.vault.target.desired.v1`; inspect, fetch, and acknowledge only its four fixed durables; and subscribe only to `_INBOX.>`. It SHALL NOT create or delete consumers, purge or delete streams, subscribe to command/event wildcards, inspect unrelated durables, or publish another subject.

#### Scenario: Exact paths work and foreign paths fail
- **WHEN** the synthetic GitHub identity exercises every required publish/fetch path and representative forbidden topology, wildcard, unrelated-durable, and foreign-publish paths
- **THEN** the six declared families work and every forbidden operation is denied by the real broker
