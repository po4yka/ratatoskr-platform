## 1. The sampler (R5-13 rule 1)

- [x] 1.1 Add `crates/eventing/tests/consumer_lag.rs` (5 published, 2 pulled, 1 acknowledged: pending, ack pending and the oldest unacked age by an injected clock against the message's own timestamp; a caught-up consumer reports 0; a missing durable is skipped; the stream gauges match `stream.info()`) against a throwaway real broker, with `sample_bus` as a stub that sets nothing, and run it RED on the assertions
- [x] 1.2 Implement `observe::sample_bus`, the nine metric constants and descriptions, the Edge-internal sample targets, call it from Edge's observer every tick, and run 1.1 GREEN

## 2. The alert rules (R5-13 rule 2)

- [x] 2.1 Add `crates/eventing/tests/bus_alert_rules.rs` (the file exists, parses as YAML, every alert has `expr`, `for`, `labels.severity` and `annotations.summary`, every series in an `expr` is a metrics constant or in the documented producer allow-list) and run it RED because the file does not exist
- [x] 2.2 Write `deploy/monitoring/ratatoskr-bus-alerts.rules.yml`, the commented producer targets in `promscrape.ratatoskr.yml` and the `deploy/README.md` section, and run 2.1 GREEN
