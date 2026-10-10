## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md sections S03 and S04. The contract text is the single source of truth for subject lists, durable names, filters, ack waits and owners; this design only records the Platform-side decisions.

## Decisions

- **One table, one function.** `FixedConsumerSpec` carries `stream`, `ack_policy`, `deliver_policy`, `ack_wait_seconds`, `max_deliver` and a `filter_subject`. `ensure_fixed_topology` creates every entry and compares every one of those fields plus "no deliver subject" and replay Instant when the durable already exists; a difference is `EventingError::Bus` and Edge refuses to start, because modifying a cursor would silently drop or replay work. The KV bucket is created with `create_key_value` (max age 24 h, direct get on).
- **A dev-only profile crate instead of string surgery in each test.** The placeholders in `ratatoskr.conf` are intentionally invalid nkeys, which nats-server 2.15 refuses to load. Tests, CI and the operator all need the same "replace the placeholders with real public keys" step, so it is one crate with a parser and a renderer. It is `publish = false`, is a dev tool, and no production binary depends on it.
- **The ACL is asserted twice.** A static test over the stanza text (ownership of each durable, no wildcard except the three allowed shapes, thirteen identities) and a real-broker matrix over the rendered deployed file (one test per identity, one generic "no identity can inspect a durable it does not own" loop). The static test catches review-time drift; the matrix catches a subject that looks right and does not work (the KV subjects were derived by reading async-nats and are first proven by the matrix).
- **Bind address.** Inside the container the server binds `0.0.0.0` so that Docker's published port works; `compose.yaml` still publishes on `127.0.0.1` only. The comment in the file says the publication, not the bind, is the boundary, and port 4222 additionally requires an nkey.

## Risks / Trade-offs

- `evt.platform.operation.reported.v1` stays one shared subject trusted by the envelope `producer` field, so the five identities allowed to publish it can forge reports for each other. Per-producer subjects need a contract change and are documented, not fixed, here.
- Services fail startup until Edge has provisioned their durable. The deploy order (reload NATS, restart Edge, start services) is documented in `deploy/nats/README.md`.

## Migration Plan

Install the new public nkeys, reload NATS, restart Edge (it provisions the table and the bucket), start the services. Rollback restores the previous conf and reloads NATS; provisioned durables are never deleted because their cursors hold work.
