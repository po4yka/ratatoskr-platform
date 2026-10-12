## Why

The repository owner's stream policy keeps `ratatoskr_events` dropping the oldest message at 7 days or 1 GiB, so a deletion fact that Knowledge does not consume in time would be lost silently while its producer already marked it published at the PubAck. XR-021 CONTRACTS.md R5-11 makes Knowledge acknowledge every deletion fact with `knowledge.deletion.acknowledged.v1` and makes each producer re-send until it hears it. That needs one durable per producer on the events stream and an ACL that lets Knowledge publish the acknowledgement and each producer read only its own copy.

Cites XR-021 CONTRACTS.md R5-11 rule 3 (fixed durables and ACL).

## What Changes

- `platform_eventing::DOMAIN_CONSUMERS` grows from 18 to 23 entries: `ratatoskr_chatgpt_deletion_acks`, `ratatoskr_claude_deletion_acks`, `ratatoskr_x_deletion_acks`, `ratatoskr_instagram_deletion_acks` and `ratatoskr_threads_deletion_acks`, all on `ratatoskr_events`, filter `evt.knowledge.deletion.acknowledged.v1`, explicit acknowledgements, deliver all, ack wait 30 s, max_deliver -1.
- `deploy/nats/ratatoskr.conf`: KNOWLEDGE gains the publish allow `evt.knowledge.deletion.acknowledged.v1`; CHATGPT, CLAUDE, X, INSTAGRAM and THREADS each gain the inspect, pull and acknowledge grants of their own durable and no other identity holds any of them.
- `deploy/nats/README.md` lists the five durables and states the residual-risk sentence of R5-11 (every producer sees every acknowledgement: ids, tenant refs and a subject reference, no content).
- Deliberate breaks: Edge creates five more durables at startup, and a broker that already holds a differing durable of one of these names makes Edge refuse to start. A producer built before this change cannot read its durable until the ACL is reloaded and Edge restarted (reload NATS, restart Edge, then the services).

## Capabilities

### Modified Capabilities

- `bus-topology`: five more fixed durables, and the ACL rows that go with them.

## Impact

- `crates/eventing/src/stream.rs`, `crates/eventing/tests/{fixed_topology,deployed_nats_config}.rs`, `services/edge/tests/{deployment_profile,boot}.rs` and `services/edge/tests/support/mod.rs`, `deploy/nats/ratatoskr.conf`, `deploy/nats/README.md`.
