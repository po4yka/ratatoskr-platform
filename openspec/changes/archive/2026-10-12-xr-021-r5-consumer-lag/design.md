## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md R5-13. The contract text is the source of truth; this file records the Platform-side decisions.

## Decisions

- **Sample from the broker, not from the database.** Lag is a property of the stream and its consumers, so the sampler asks JetStream. It reuses Edge's own connection and the grants Edge already has (`$JS.API.>`): no new permission, no new port.
- **Age of the first unacked message.** `oldest_unacked_age_seconds` is `now` minus the stream timestamp of the first message after `ack_floor.stream_sequence` whose subject matches the consumer's filter. async-nats 0.50 exposes this request as `raw_message_builder().sequence(n).next_by_subject(filter)`, which sends `STREAM.MSG.GET` with `{"seq": n, "next_by_subj": filter}`; `NoMessageFound` means caught up and yields 0. The clock is injected (`now`), so the test asserts against the message's own stream timestamp.
- **The table is the label set.** Series are labelled with `stream` and `durable` and nothing else, so cardinality is bounded by the durable table (32 targets, under the 40 the contract allows). Edge's own three consumers (`platform_edge_projection`, `platform_schedule_registration`, `platform_stream_erasure`) are listed as sample targets in `stream.rs` but are not part of the tables Edge creates from.
- **A missed sample keeps the last value.** The gauge is only set on success; a failure is a `warn!` and a gap, not a zero, so an alert on `oldest_unacked_age_seconds` cannot be silenced by a broker blip. A durable that does not exist yet is skipped without a log above `debug`.
- **Rules are a Prometheus rule file.** vmalert and Prometheus both load it; which engine runs it on the host is the owner's choice. A text test checks that every series in an `expr` is either a constant of `metrics.rs` or in the file's documented allow-list of producer-side names (`*_oldest_unacknowledged_tombstone_age_seconds`).
- **Thresholds.** The events stream keeps 7 days, and the ingest durables wait 120 s per message, so one hour of lag is already 30 messages of a slow LLM call and is worth a warning, two days is more than a quarter of the retention and is critical, and 80 percent of the byte cap is the point at which the drop-oldest policy is close.

## Risks / Trade-offs

- The producer-side alert (`RatatoskrUnacknowledgedTombstones`) needs scrape targets on operator ports that stay host-only; the targets ship commented out.
- One `MSG.GET` per lagging durable per tick (15 s) costs a broker round trip each; durables that are caught up cost none.
