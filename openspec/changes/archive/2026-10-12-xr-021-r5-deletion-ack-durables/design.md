## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md R5-11. The contract text is the source of truth; this file records the Platform-side decisions.

## Decisions

- **One durable per producer, not one shared durable.** Each of the five producers must see every acknowledgement and ack or ignore it on its own cursor. A shared durable would let one producer's ack consume the message another still needs. The filter is the same subject for all five, which JetStream allows.
- **The table owns the shape.** The five specs use `FixedConsumerSpec::standard`, so they inherit the S04 default (explicit, deliver all, 30 s, unlimited redelivery). `fixed_topology.rs` writes the expected rows out independently of the constants, so a rename or a changed filter fails there.
- **ACL by TRIO, nothing wider.** Each producer gets exactly the three subjects of its own durable (`CONSUMER.INFO`, `CONSUMER.MSG.NEXT`, `$JS.ACK.<stream>.<durable>.>`). The static test keeps S03 invariants 1 to 4 and the real-broker matrix proves that a producer can describe, fetch and acknowledge its own durable and is refused on a sibling's: `assert_only_own_durables` already iterates every durable in its owner table, so the five new rows are covered by adding them to it.
- **Knowledge publishes, nobody else does.** `evt.knowledge.deletion.acknowledged.v1` is added to KNOWLEDGE only. The matrix feeds each new durable through the KNOWLEDGE identity.
- **Edge-internal durables stay out of the table.** `platform_edge_projection`, `platform_schedule_registration` and (R5-12) `platform_stream_erasure` are created by Edge's own consumer.

## Risks / Trade-offs

- Every producer reads every tenant's acknowledgement (ids and refs, no content). This is the sentence the README now carries; per-tenant filtering would need a subject per tenant and is not in scope.
- A broker that already holds a durable of one of these names with another shape stops Edge at startup. Nothing is deployed yet, so the break is named and not staged.
