## Why

ChatGPT and Claude put the full text of a conversation on the wire in `evt.ai_archive.*` facts, and those facts stay in `ratatoskr_events` for up to 7 days after the owner deletes them. The deployed ACL denies Edge `STREAM.PURGE` and `STREAM.DELETE` and no producer holds any stream-management permission, so nothing can remove them. XR-021 CONTRACTS.md R5-12 lets a producer ask Edge to erase single stream messages that the producer itself published for the owner it names, and nothing else.

Cites XR-021 CONTRACTS.md R5-12 rules 4 and 5.

## What Changes

- `platform_eventing::stream_erasure` implements `consumer::Handler` for `cmd.platform.stream_erasure.requested.v1`. Inside the inbox transaction it validates the command envelope (producer `ratatoskr-chatgpt` or `ratatoskr-claude`, `tenant_id` equals `payload.owner`, `aggregate_id` equals `payload.request_ref`, payload validates) and Terms anything else. For each sequence it reads the stream message and erases it only when its subject is one of the seven content-bearing `evt.ai_archive.*` subjects (never the tombstone), its decoded envelope producer equals the command's producer and its tenant equals `payload.owner`. A missing message is `missing`, anything else is `refused` and left alone. It writes one content-free audit row per command and counts `platform_stream_erasure_total{outcome}`. A transient broker failure is a Nak.
- `ratatoskr-edge` spawns the consumer `platform_stream_erasure` (commands stream, filter `cmd.platform.stream_erasure.requested.v1`, explicit, 30 s) next to `platform_schedule_registration`.
- `deploy/nats/ratatoskr.conf`: CHATGPT and CLAUDE gain the publish allow `cmd.platform.stream_erasure.requested.v1`. EDGE is unchanged (its `$JS.API.>` already covers `STREAM.MSG.GET` and `STREAM.MSG.DELETE`; its deny of `STREAM.DELETE` and `STREAM.PURGE` stays). New invariant 7: no stanza other than EDGE holds a publish allow that starts with `$JS.API.STREAM.`.
- `deploy/nats/README.md`: the residual-risk paragraph gains the sentence about what a compromised ChatGPT or Claude identity can now ask for.
- Deliberate breaks: none on the wire for existing messages. Edge needs one more durable and CHATGPT and CLAUDE one more publish grant (reload NATS, then restart Edge).

## Capabilities

### Modified Capabilities

- `bus-topology`: the erasure command, its handler and invariant 7.

## Impact

- `crates/eventing/src/{stream_erasure.rs,lib.rs}`, `crates/telemetry/src/metrics.rs`, `crates/eventing/tests/stream_erasure.rs`, `crates/eventing/tests/deployed_nats_config.rs`, `services/edge/tests/{deployment_profile,support}`, `services/edge/src/main.rs`, `deploy/nats/{ratatoskr.conf,README.md}`.
