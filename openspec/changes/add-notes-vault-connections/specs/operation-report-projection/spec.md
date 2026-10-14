## ADDED Requirements

### Requirement: Only the notes lane reports for a connect operation

The report authority table SHALL list `ratatoskr-notes` as the only producer for `notes.connection.connect`, and the connect proxy SHALL record `ratatoskr-notes` as the reporter of the operation it mints.

#### Scenario: Another lane reports on a connect operation

- **WHEN** the X identity publishes a report with the owner's tenant for a `notes.connection.connect` operation
- **THEN** Platform rejects the report and the status is unchanged, and the same report from `ratatoskr-notes` is applied, verified by `crates/operations/tests/report_authority.rs::notes_connect_accepts_only_the_notes_lane`

### Requirement: Concurrent writers of one operation record one transition

The one transition applier SHALL lock the row of the operation before it reads the status, so that two writers that apply the same terminal status to one operation at the same time record one transition and one error.

#### Scenario: Two writers fail one operation at the same time

- **WHEN** one transaction applies `failed` to an operation and records an error, and a second transaction does the same before the first one commits
- **THEN** the second transaction waits for the first one, gets `Duplicate`, and the operation holds one `failed` progress entry and one error, verified by `crates/operations/tests/lifecycle.rs::two_concurrent_failures_record_one_transition_and_one_error`
