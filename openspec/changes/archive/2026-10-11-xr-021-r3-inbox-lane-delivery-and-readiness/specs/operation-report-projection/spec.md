## ADDED Requirements

### Requirement: An operation records the service it was dispatched to

`operations.operations` SHALL carry a nullable `reporter` that is a producer name. `platform_operations::accept` SHALL take the reporter, SHALL refuse one that is not in the allow-list of the operation's kind, and every production caller SHALL pass the lane the work was dispatched to: the extractor for a URL capture, a blob capture and the webhook content-capture target, `ratatoskr-x`, `ratatoskr-instagram` or `ratatoskr-threads` for a social capture of that provider, `ratatoskr-channel-digests` for the channel-digest routes, the schedule's `service_name` for a scheduled occurrence, and none for an AI-archive operation. For a kind with a listed allow-list, Platform SHALL apply a report only when the operation's recorded reporter is non-null and equals the report producer.

#### Scenario: another lane's report is rejected

- **WHEN** an operation is accepted with reporter `ratatoskr-instagram` and `ratatoskr-x` reports for it with the owner's tenant
- **THEN** the report is rejected and the operation status does not change

#### Scenario: the recorded reporter's report is applied

- **WHEN** an operation is accepted with reporter `ratatoskr-instagram` and `ratatoskr-instagram` reports for it with the owner's tenant
- **THEN** the report is applied

#### Scenario: a listed kind without a recorded reporter rejects every producer

- **WHEN** an operation of a listed kind was accepted without a reporter and any allowed producer reports for it
- **THEN** the report is rejected

#### Scenario: accept refuses a reporter outside the kind's list

- **WHEN** `accept` is called with a reporter that is not in the allow-list of the kind
- **THEN** it returns an error and records no operation

#### Scenario: every intake path records its lane

- **WHEN** a URL capture, a blob capture, a social capture of each provider, a channel-digest request, a webhook content capture or a scheduled occurrence is accepted
- **THEN** the stored reporter is the lane named above, and an AI-archive operation stores none
