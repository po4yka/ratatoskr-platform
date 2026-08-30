## Why

Platform owns the fleet JetStream topology and NATS authorization, but it provisions no GitHub durables or GitHub application identity. GitHub Catalog therefore cannot consume scheduled sync, Knowledge outcomes, or Vault acknowledgements and cannot publish its two existing cross-service messages without broader authority than it needs.

## What Changes

- Provision four fixed GitHub durables on the existing bounded command and event streams.
- Add one GitHub NKey public-key placeholder with exact publish, consumer-info, fetch, acknowledgement, and reply-inbox permissions.
- Refuse drifted existing consumers instead of mutating them, and keep every topology-changing JetStream API outside the application identity.
- Extend the synthetic real-broker permission test and deployment documentation without adding a production seed.

## Capabilities

### New Capabilities

- `github-bus-topology`: Platform-owned fixed consumers and least-privilege NATS authorization for GitHub Catalog.

### Modified Capabilities

None.

## Impact

This is the additive first repository in workspace changeset `GHB-017`. It changes `platform-eventing` topology tests/provisioning and `deploy/nats` configuration, fixtures, and documentation. GitHub remains unable to create, delete, purge, or widen topology. No shared payload contract or database schema changes.
