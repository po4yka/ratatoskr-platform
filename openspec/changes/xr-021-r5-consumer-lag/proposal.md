## Why

The repository owner accepts the 7 day and 1 GiB drop-oldest retention of `ratatoskr_events` and asks for visibility instead of a bigger store. Today `crates/eventing/src/observe.rs` samples only the outbox and inbox tables, no series describes a durable's lag, and `deploy/README.md` says no alert rule watches any series. A consumer that falls behind by days would be silent until its messages were dropped.

Cites XR-021 CONTRACTS.md R5-13.

## What Changes

- `platform_eventing::observe::sample_bus` reads every fixed durable (the fixed table, the Telegram, AI-archive-report, social-capture durables and Edge's own three) and both streams through the JetStream API Edge already holds, and publishes `platform_bus_consumer_{pending,ack_pending,redelivered,oldest_unacked_age_seconds}{stream,durable}` and `platform_bus_stream_{messages,bytes,max_bytes,oldest_message_age_seconds}{stream}`. The oldest unacked age comes from the message at the first sequence after the consumer's `ack_floor` that matches its filter, read with a raw `MSG.GET` using `next_by_subj`, and is 0 when the consumer is caught up. A failed sample logs a warning and keeps the last value; a durable that does not exist is skipped.
- Edge's observer calls it every tick.
- `deploy/monitoring/ratatoskr-bus-alerts.rules.yml` ships the six alerts of R5-13 with thresholds and their reasoning; `promscrape.ratatoskr.yml` carries the commented producer scrape targets; `deploy/README.md` replaces the "Alert rules" bullet with the file and the statement that the owner chooses the rule engine.
- Deliberate breaks: none. Nine metric names are added to `platform_telemetry::metrics::ALL`.

## Capabilities

### Modified Capabilities

- `bus-topology`: consumer lag and stream state are exported, and an alert rule file ships.

## Impact

- `crates/eventing/src/observe.rs`, `crates/eventing/src/stream.rs` (the three Edge-internal durables as sample targets), `crates/telemetry/src/metrics.rs`, `crates/telemetry/tests/subscriber.rs`, `crates/eventing/tests/{consumer_lag,bus_alert_rules}.rs`, `services/edge/src/main.rs`, `deploy/monitoring/*`, `deploy/README.md`.
