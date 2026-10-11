## Why

The round 2 report authority rule (CONTRACTS.md R2-02) requires an envelope `tenant_id` equal to `user:<owner>` on every report, including `ai_archive.import`. Neither AI archive service puts a `tenant_id` in its operation report envelope: `operation_report_envelope` in `ratatoskr-chatgpt` and `ratatoskr-claude` builds the envelope without one, stores it, and the outbox pump publishes the stored document unchanged. With the rule as first landed, every ChatGPT and Claude import report on `evt.ai-archive.<provider>.operation.reported.v1` is rejected and the import operation never leaves `accepted`, `running` or `queued`. The earlier Platform tests built their envelopes with a tenant, which hid the gap.

Cites XR-021 CONTRACTS.md R2-02.

## What Changes

- `authority::authorize` takes an `acceptance_bound` flag. For a `ProviderBound` kind, an envelope with no `tenant_id` is accepted when Platform holds an acceptance record for the operation that names the operation's owner. A `tenant_id` that is present is still checked against the owner, for every kind. A listed kind still needs the tenant.
- `ProgressProjection` loads the acceptance record before authorizing and counts it as a binding only when its `owner_user_id` is the operation's owner. The provider subject and producer check is unchanged.
- New tests feed a report in the exact shape the AI archive services publish today (no `tenant_id`, provider subject, provider producer), for both providers.
- Deliberate relaxation of the CONTRACTS.md R2-02 wording for `ai_archive.import` only. The ChatGPT and Claude services are not changed here; if they later add a tenant, the check applies to it.
- No database change. No version change.

## Capabilities

### Modified Capabilities

- `operation-report-projection`: an AI-archive import report is bound to its owner by the acceptance record when its envelope has no tenant.

## Impact

- `crates/operations/src/{authority,projection}.rs`, `crates/operations/tests/report_authority.rs`, `docs/ARCHITECTURE.md`.
- CONTRACTS.md R2-02 needs the matching amendment in the workspace; this repository cannot edit it.
