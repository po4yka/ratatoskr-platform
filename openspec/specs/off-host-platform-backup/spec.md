# Off-host Platform Backup Specification

## Purpose

Preserve Platform state and its restore configuration outside the Raspberry Pi, with a recovery
copy that is confidential, independently retained, and proved restorable on a separate host.

## Requirements

### Requirement: Completed Platform backup material is replicated confidentially

The deployment SHALL upload an immutable, age-encrypted off-host recovery set after the daily dump
and local Borg archive complete. A recovery set MUST contain the completed PostgreSQL custom dump,
the selected Borg archive export, and the allowlisted Platform deployment configuration. The
replication job MUST reject an incomplete input, MUST not upload an unencrypted recovery payload,
and MUST leave no private age identity or S3 credential in the repository or in the uploaded
configuration snapshot.

#### Scenario: A completed daily recovery set is uploaded

- **WHEN** a valid daily dump, a completed Borg archive, and all required configuration files are
  available to the replication job
- **THEN** the S3-compatible target receives encrypted objects for all three recovery materials
  under that day's immutable prefix

#### Scenario: Missing or incomplete input fails before an upload

- **WHEN** a required dump, Borg archive, or configuration source is missing or unfinished
- **THEN** the replication job exits nonzero and the target receives no object for that recovery
  material

#### Scenario: A stale dump is not relabeled as today's recovery set

- **WHEN** the replication timer runs for a UTC date but the newest completed dump has a different
  UTC date in its generated filename
- **THEN** the replication job exits nonzero and uploads no object under the current day's prefix

#### Scenario: A configuration snapshot excludes recovery credentials

- **WHEN** the replication job creates the encrypted configuration snapshot
- **THEN** its plaintext archive contains the allowlisted Platform configuration and excludes the
  S3 and age recovery environment files

### Requirement: Remote retention is independent and bounded

The deployment SHALL provide a bucket lifecycle policy recommendation whose expiration windows are
independent of the fourteen local dumps and retain remote daily recovery sets for at least ninety
days. The policy MUST abort incomplete multipart uploads and MUST cover every recovery-object
prefix created by the replication job.

#### Scenario: Remote lifecycle policy covers all recovery material

- **WHEN** the generated lifecycle policy is inspected with a ninety-day remote retention setting
- **THEN** dump, Borg, and configuration object prefixes expire after ninety days and incomplete
  multipart uploads expire on the documented shorter window

#### Scenario: Changing local dump retention does not shorten remote retention

- **WHEN** the local dump keep count is changed while the remote retention setting remains ninety
  days
- **THEN** the generated remote lifecycle policy continues to retain each remote recovery prefix
  for ninety days

### Requirement: Off-host recovery is tested by a separate verifier

An off-host verifier host SHALL run a weekly systemd timer that retrieves the previous day's remote
dump object, decrypts it with its locally held age identity, restores it into a newly created scratch
PostgreSQL database, verifies the Platform schemas and constraints, removes the scratch database,
and reports a single unambiguous success or failure result. The Raspberry Pi MUST not store that
private age identity.

#### Scenario: Yesterday's remote dump restores successfully

- **WHEN** the verifier finds yesterday's encrypted dump and its configured identity can decrypt it
- **THEN** it restores the dump into a scratch PostgreSQL database, validates the required Platform
  schemas and constraints, removes the scratch database, and exits with a success result

#### Scenario: A missing or undecryptable remote dump fails the drill

- **WHEN** yesterday's object is missing, corrupt, or cannot be decrypted by the verifier identity
- **THEN** the drill exits nonzero, prints its failing stage, and does not report a successful
  restore

### Requirement: A new board can recover from off-host material alone

The runbook SHALL describe a restore to a replacement board using only the remote recovery set and
operator-held recovery credentials. It MUST restore the configuration snapshot before service start,
restore the PostgreSQL dump before enabling Platform services, and state that continuous WAL
shipping and multi-cloud replication are intentionally absent.

#### Scenario: The documented replacement-board procedure is dry-run validated

- **WHEN** the runbook's non-destructive preparation and download commands are run with its dry-run
  environment
- **THEN** they resolve the remote recovery set and planned restore inputs without writing a live
  database or contacting an undeclared destination
