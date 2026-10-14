## ADDED Requirements

### Requirement: Only the notes lane reports for a connect operation

The report authority table SHALL list `ratatoskr-notes` as the only producer for `notes.connection.connect`, and the connect proxy SHALL record `ratatoskr-notes` as the reporter of the operation it mints.

#### Scenario: Another lane reports on a connect operation

- **WHEN** the X identity publishes a report with the owner's tenant for a `notes.connection.connect` operation
- **THEN** Platform rejects the report and the status is unchanged, and the same report from `ratatoskr-notes` is applied, verified by `crates/operations/tests/report_authority.rs::notes_connect_accepts_only_the_notes_lane`
