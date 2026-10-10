## Why

Platform owns the only deployed NATS ACL (`deploy/nats/ratatoskr.conf`) and the only code that provisions durable consumers (`crates/eventing/src/stream.rs`, created by Edge at startup). Today the ACL does not parse under nats-server 2.15 (the placeholder nkeys are not valid public keys), binds inside the container to loopback so Docker's published port refuses connections, has no identity for the extractor, the browser worker, Knowledge, GitHub, Vault or channel-digests, lets the extractor hold `evt.>` and `$JS.API.>`, and Edge provisions only the social, Telegram and AI-archive durables. The services of the fleet therefore cannot connect, cannot find their durables, and in one case hold far more authority than they need.

Cites XR-021 CONTRACTS.md sections S03 (identities and permissions), S04 (fixed durables and the KV bucket) and S02 rule 6 (consumers verify, never create).

## What Changes

- Move every `ratatoskr-*` contracts dependency to contracts commit `ad16855c4e7f3d52cd118274faa3b8f3ab4da576` and add the AI-archive, document and channel-digest contract crates.
- New dev-only crate `platform-nats-profile` (`publish = false`): parses the stanzas of `ratatoskr.conf`, renders a copy with valid generated user nkeys, and ships the `render-nats-config` binary used by tests and CI.
- `FixedConsumerSpec` gains stream, ack policy, deliver policy, ack wait and max deliver; `DOMAIN_CONSUMERS` and the `browser_worker_completions` KV bucket join the table; one `ensure_fixed_topology` / `ensure_domain_topology` function provisions all of it and the old per-family wrappers are folded into it (no compatibility shims).
- Edge provisions the whole table after declaring the streams.
- `ratatoskr.conf` becomes the thirteen-identity ACL of S03, binds `0.0.0.0` inside the container (the publication is the boundary), and the operator guide documents the identities, seeds, deploy order and the shared-subject residual risk.
- A real-broker permission matrix proves every grant and refusal against the deployed file, and CI smoke boots Edge against the rendered deployed config.
- No database change. No version change. Nothing in production depends on `platform-nats-profile`.

## Capabilities

### New Capabilities

- `bus-topology`: the fixed durable table, the KV bucket and the NATS identity rules that Edge and the deployed ACL must satisfy.

## Impact

- `Cargo.toml`, `Cargo.lock`, new `crates/nats-profile`, `crates/eventing/src/stream.rs` and its tests, `services/edge/src/main.rs` and tests, `deploy/nats/*`, `deploy/README.md`, `.github/workflows/ci.yml`.
- Deliberate breaks: the `ensure_social_capture_consumers`, `ensure_telegram_notification_consumer` and `ensure_ai_archive_report_consumers` functions are removed in favour of one function; the extractor's old `evt.>` / `$JS.API.>` fragment is deleted from the fleet and replaced by two narrow stanzas.
