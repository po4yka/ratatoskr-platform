## ADDED Requirements

### Requirement: Edge exports consumer lag and stream state

Edge's observer SHALL, on every tick, publish for each fixed durable it knows (the fixed table, the Telegram, AI-archive-report and social-capture durables, and Edge's own consumers) `platform_bus_consumer_pending`, `platform_bus_consumer_ack_pending`, `platform_bus_consumer_redelivered` and `platform_bus_consumer_oldest_unacked_age_seconds`, labelled with `stream` and `durable`, and for each of the two streams `platform_bus_stream_messages`, `platform_bus_stream_bytes`, `platform_bus_stream_max_bytes` and `platform_bus_stream_oldest_message_age_seconds`, labelled with `stream`. The oldest unacked age SHALL be the age of the first message after the consumer's acknowledgement floor that matches its filter, and 0 when the consumer is caught up. A failed sample SHALL keep the last value, and a durable that does not exist SHALL be skipped without an error.

#### Scenario: lag rises and falls with acknowledgements

- **WHEN** five messages are published, two are pulled and one is acknowledged
- **THEN** the pending count is what the server reports for the consumer, the ack pending count is 1 and the oldest unacked age is the age of the second message by its own stream timestamp

#### Scenario: a caught-up consumer reports zero

- **WHEN** every message of a consumer has been acknowledged
- **THEN** its pending, ack pending and oldest unacked age are 0

#### Scenario: a missing durable is skipped

- **WHEN** the sampler is given a durable that does not exist on the broker
- **THEN** it records nothing for it and does not fail

### Requirement: A bus alert rule file ships with the deployment

`deploy/monitoring/ratatoskr-bus-alerts.rules.yml` SHALL define the alerts `RatatoskrKnowledgeConsumerLag`, `RatatoskrKnowledgeConsumerNearRetention`, `RatatoskrKnowledgeConsumerBacklog`, `RatatoskrEventsStreamNearCapacity`, `RatatoskrConsumerRedeliveryStorm` and `RatatoskrUnacknowledgedTombstones`, each with `expr`, `for`, a `severity` label and a `summary` annotation, and every series in an expression SHALL be a constant of `platform_telemetry::metrics` or a documented producer-side name.

#### Scenario: the rule file agrees with the metric names

- **WHEN** the rule file is parsed
- **THEN** every alert is complete and every series name it uses is exported by Edge or listed in the file's producer allow-list
