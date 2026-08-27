## Purpose

Defines the public operation-result projection for privacy-safe AI archive import completeness.

## ADDED Requirements

### Requirement: Platform preserves bounded archive import summaries

Platform SHALL persist and return a valid `ai_archive.import` result with its typed
`ai_archive_import_summary` unchanged, without interpreting archive content or diagnostics.

#### Scenario: A producer reports a complete archive import

- **WHEN** Platform receives a valid terminal operation report containing an archive import summary
- **THEN** the owning caller's operation snapshot contains the same bounded result summary

### Requirement: Contract-valid entity references are accepted

Platform SHALL accept a result target that conforms to the canonical `EntityRef` grammar.

#### Scenario: An archive target uses an underscore in its kind

- **WHEN** a valid archive import result targets `ai_archive:<uuid>`
- **THEN** Platform persists the result rather than rejecting its entity reference
