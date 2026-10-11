## ADDED Requirements

### Requirement: A report is applied only for the owner's tenant and an allowed producer

Platform SHALL load the operation a progress report names before applying it and SHALL reject the report, recording it in the inbox and leaving the status unchanged, unless the envelope `tenant_id` is present and equals `user:<owner>` of that operation and the report producer is in the allow-list of the operation's kind. The allow-list SHALL be one table in `platform_operations`; a kind absent from the table allows no producer. `ai_archive.import` keeps its provider-bound subject and producer check.

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

- **WHEN** a report arrives whose envelope has no `tenant_id`
- **THEN** the report is rejected

#### Scenario: a kind with no row is rejected

- **WHEN** a report names an operation whose kind is not in the table
- **THEN** the report is rejected for every producer
