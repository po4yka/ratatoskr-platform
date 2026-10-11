# Operation report projection

## Purpose

Defines how Platform turns complete domain operation reports into durable and truthful public
operation snapshots without taking ownership of result content.

## Requirements

### Requirement: Reported results round-trip through Platform

Platform SHALL preserve the complete structured result references carried by a valid v1 operation
report and SHALL return them unchanged in the public operation snapshot.

#### Scenario: a BlobRef round-trips

- **WHEN** Platform receives a successful report with a result target and `BlobRef`
- **THEN** the operation snapshot contains the same target, owner, digest, media type, and byte length

### Requirement: Reported diagnostics round-trip through Platform

Platform SHALL preserve the complete typed error and warning envelopes carried by a valid v1
operation report and SHALL return them in the public operation snapshot.

#### Scenario: a failed report remains valid

- **WHEN** Platform receives a failed report with an error and explicit retryability
- **THEN** the operation snapshot is readable and contains the same error and retryability

#### Scenario: warning details round-trip

- **WHEN** Platform receives a report with a warning containing a field path or additive v1 data
- **THEN** the operation snapshot contains that warning data unchanged

### Requirement: One report is applied atomically

Platform SHALL commit or roll back the status, results, errors, and warnings from one report together
with its inbox record.

#### Scenario: persistence fails during report application

- **WHEN** any durable write required by a report fails
- **THEN** no partial status, result, diagnostic, or processed-inbox state from that report is committed

### Requirement: A report is applied only for the owner's tenant and an allowed producer

Platform SHALL load the operation a progress report names before applying it and SHALL reject the report, recording it in the inbox and leaving the status unchanged, unless the envelope `tenant_id` is present and equals `user:<owner>` of that operation and the report producer is in the allow-list of the operation's kind. The allow-list SHALL be one table in `platform_operations`; a kind absent from the table allows no producer. `ai_archive.import` keeps its provider-bound subject and producer check, and an `ai_archive.import` report whose envelope has no `tenant_id` SHALL be accepted when Platform's acceptance record for the operation names the operation's owner. A `tenant_id` that is present SHALL be checked against the owner for every kind.

#### Scenario: another tenant's report is rejected

- **WHEN** an allowed producer reports for an operation with an envelope tenant that is not the owner
- **THEN** the report is rejected and the operation status does not change

#### Scenario: a producer outside the kind's list is rejected

- **WHEN** a producer that is not in the allow-list of the operation's kind reports with the owner's tenant
- **THEN** the report is rejected and the operation status does not change

#### Scenario: an allowed producer with the owner's tenant is applied

- **WHEN** an allowed producer reports with the owner's tenant
- **THEN** the report is applied

#### Scenario: a report without a tenant is rejected

- **WHEN** a report arrives whose envelope has no `tenant_id` for a kind other than a bound `ai_archive.import`
- **THEN** the report is rejected

#### Scenario: a kind with no row is rejected

- **WHEN** a report names an operation whose kind is not in the table
- **THEN** the report is rejected for every producer

#### Scenario: a bound AI archive report without a tenant is applied

- **WHEN** the provider producer reports on its provider subject for an `ai_archive.import` operation whose acceptance record names the owner, in an envelope with no `tenant_id`
- **THEN** the report is applied

#### Scenario: an AI archive report naming another tenant is rejected

- **WHEN** the same report carries a `tenant_id` that is not the owner's
- **THEN** the report is rejected and the operation status does not change

#### Scenario: an import with no usable acceptance record needs the tenant

- **WHEN** an `ai_archive.import` report with no `tenant_id` arrives for an operation that has no acceptance record, or one that names another owner
- **THEN** the report is rejected
