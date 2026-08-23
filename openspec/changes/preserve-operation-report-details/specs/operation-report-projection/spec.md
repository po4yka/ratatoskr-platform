## Purpose

Defines how Platform turns complete domain operation reports into durable and truthful public operation snapshots without taking ownership of result content.

## ADDED Requirements

### Requirement: Reported results round-trip through Platform

Platform SHALL preserve the complete structured result references carried by a valid v1 operation report and SHALL return them unchanged in the public operation snapshot.

#### Scenario: a BlobRef round-trips

- **WHEN** Platform receives a successful report with a result target and `BlobRef`
- **THEN** the operation snapshot contains the same target, owner, digest, media type, and byte length

### Requirement: Reported diagnostics round-trip through Platform

Platform SHALL preserve the complete typed error and warning envelopes carried by a valid v1 operation report and SHALL return them in the public operation snapshot.

#### Scenario: a failed report remains valid

- **WHEN** Platform receives a failed report with an error and explicit retryability
- **THEN** the operation snapshot is readable and contains the same error and retryability

#### Scenario: warning details round-trip

- **WHEN** Platform receives a report with a warning containing a field path or additive v1 data
- **THEN** the operation snapshot contains that warning data unchanged

### Requirement: One report is applied atomically

Platform SHALL commit or roll back the status, results, errors, and warnings from one report together with its inbox record.

#### Scenario: persistence fails during report application

- **WHEN** any durable write required by a report fails
- **THEN** no partial status, result, diagnostic, or processed-inbox state from that report is committed
