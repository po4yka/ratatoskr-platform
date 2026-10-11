## Why

Independent verifiers traced the XR-021 round 1 flows through the pushed code and found defects inside Platform. Any identity allowed to publish `evt.platform.operation.reported.v1` can terminate another tenant's operation, because `ProgressProjection` binds a report to its operation only for AI-archive operations. The deployed NATS file keeps the 1 MiB default `max_payload`, so large archive and document facts are refused on every attempt. A schedule registration is accepted for an owner that is not a user. The social capture route rejects `threads.com` permalinks that the Threads service accepts. The chunk route can answer 413 but its OpenAPI does not say so. The NATS README names the wrong seed variables.

Cites XR-021 CONTRACTS.md sections R2-02 (report authority), R2-04 rule 1 (broker payload limit), R2-14 (schedule owner), R2-11 (provider permalink hosts), R2-18 (chunk 413), R2-10 rule 1 (seed variables) and R2-01 (listener binding, documentation only).

## What Changes

- New `crates/operations/src/authority.rs` holds the one `const` table of operation kind to allowed report producers and the `authorize` function. `ProgressProjection` loads the operation inside the handler transaction and rejects a report whose envelope tenant is not `user:<owner>` or whose producer is not allowed for the kind. The AI-archive provider-bound check is unchanged.
- `deploy/nats/ratatoskr.conf` sets `max_payload: 8MB`.
- `RegistrationHandler` rejects a registration whose owner is absent from `identity.users` or not active, with an audit event.
- `is_provider_permalink` accepts `threads.com` and `www.threads.com` for the Threads provider.
- `CHUNK_DOC` documents the 413 and `openapi/openapi.json` is regenerated.
- `deploy/nats/README.md` names the real seed variables; `deploy/README.md` and `deploy/systemd/edge.conf.example` state which service binds which Edge upstream.
- Deliberate break: a progress report without `tenant_id` in its envelope, or from a producer that is not in the table for the operation's kind, is now rejected. Existing projection tests that reported without a tenant now carry the owner's tenant.
- No database change. No version change.

## Capabilities

### Modified Capabilities

- `operation-report-projection`: reports are applied only for the owner's tenant and an allowed producer.
- `bus-topology`: the deployed broker accepts envelopes up to 8 MiB.
- `schedule-registration`: the owner must be an active user.
- `content-capture`: Threads permalinks on `threads.com` are accepted.

## Impact

- `crates/operations/src/{authority,projection,lib}.rs` and tests, `crates/eventing/tests/{deployed_max_payload,deployed_nats_config}.rs`, `crates/scheduling/src/registration.rs` and tests, `crates/public-api/src/{captures.rs,archives/docs.rs}` and tests, `openapi/openapi.json`, `deploy/nats/*`, `deploy/README.md`, `deploy/systemd/edge.conf.example`, `compose.yaml`, ADR text.
- Residual risk, unchanged in kind: the four extraction and social producers can still report for each other's capture operations, because a capture operation does not record which service it was dispatched to. Per-producer report subjects stay a contract follow-up.
