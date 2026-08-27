## Why

Platform's operation projection rejected the canonical `ai_archive` entity reference, so an
otherwise valid producer report could not become the operation result that an export agent polls.

## What Changes

- Pin Platform to the published bounded AI archive operation-summary contract.
- Accept canonical entity references in persisted operation results and preserve the typed summary
  from an inbound report through the public operation snapshot.

## Capabilities

### New Capabilities

- `operations/result-projection`: Durable public projection of bounded AI archive import results.

### Modified Capabilities

- None.

## Impact

`schema.sql`, operation projection integration tests, and the pinned `ratatoskr-contracts` revision
change. The public operation response gains only the additive typed contract field; no API major,
migration, parser, or archive-content handling is introduced.
