# bus-topology Specification

## Purpose
Defines the fixed durable consumers and KV bucket that Edge provisions on the Platform-owned streams, and the NATS identity rules of the deployed ACL, so that every service of the fleet can find its durable and none can reach beyond it.

## Requirements

### Requirement: Edge provisions every fixed durable and the browser-worker bucket

Edge SHALL create, after declaring the command and event streams, every durable consumer of the fixed table with the stream, filter, ack policy, deliver policy, ack wait and max-deliver the table states, with no deliver subject and instant replay, and the `browser_worker_completions` key-value bucket with a 24 hour maximum age. A durable that already exists with different settings SHALL make provisioning fail with a bus error rather than be modified.

#### Scenario: a fresh broker gets the whole table

- **WHEN** Edge provisions the topology on a broker holding only the two streams
- **THEN** every durable of the table exists with exactly its specified settings and the bucket exists with a 24 hour maximum age, and a second provisioning changes nothing

#### Scenario: a conflicting durable stops provisioning

- **WHEN** a durable of the table already exists with a different filter subject
- **THEN** provisioning fails with a bus error and the durable is left unchanged

### Requirement: The deployed ACL grants each service only its own durables

`deploy/nats/ratatoskr.conf` SHALL define exactly thirteen identities, grant `$JS.API.>`, `cmd.>` and undurable `$JS.ACK.>` only to Edge, allow no identity other than Edge to create or update consumers or streams or to read stream messages directly, and grant each fixed durable's inspect, fetch and acknowledge subjects to exactly one owner identity.

#### Scenario: an identity cannot inspect a durable it does not own

- **WHEN** any non-Edge identity requests consumer info for a fixed durable owned by another identity on a real broker running the deployed file
- **THEN** the request is not answered

#### Scenario: the deployed file is reachable through a published port

- **WHEN** the rendered deployed file runs in a container whose client port is published to the host
- **THEN** a client holding the Edge seed completes a JetStream account-info request within ten seconds

### Requirement: The deployed broker accepts envelopes up to 8 MiB

`deploy/nats/ratatoskr.conf` SHALL set `max_payload: 8MB` at the top level so that archive and document facts, which are inline envelopes, can be published. JetStream stream message size limits SHALL stay unchanged.

#### Scenario: the deployed file sets the limit

- **WHEN** the comment-stripped deployed file is read
- **THEN** it holds a top-level `max_payload: 8MB`

#### Scenario: a four mebibyte envelope is acknowledged

- **WHEN** the X identity publishes a 4 MiB envelope on `evt.platform.operation.reported.v1` to a broker running the rendered deployed file
- **THEN** the publish is acknowledged

### Requirement: Each non-Edge identity subscribes only to its own inbox prefix

`deploy/nats/ratatoskr.conf` SHALL give each of the twelve non-Edge identities `subscribe: { allow: ["_INBOX_<name>.>"] }` where `<name>` is the identity name in lower case with its underscores kept, and nothing else. Edge SHALL keep `_INBOX.>`. A service SHALL connect with `custom_inbox_prefix` set to its own prefix.

#### Scenario: the static ACL names only the own prefix

- **WHEN** the comment-stripped deployed file is parsed
- **THEN** every non-Edge stanza has a single subscribe entry equal to its row of the prefix table, and the Edge stanza has `_INBOX.>`

#### Scenario: a service cannot read the shared inbox namespace

- **WHEN** any non-Edge identity subscribes to `_INBOX.>` on a broker running the rendered deployed file
- **THEN** the client's event callback receives a permissions violation for the subscription

#### Scenario: a service cannot read another service's inbox prefix

- **WHEN** any non-Edge identity subscribes to another identity's prefix
- **THEN** the client's event callback receives a permissions violation for the subscription

#### Scenario: a service completes a JetStream request on its own prefix

- **WHEN** an identity connects with `custom_inbox_prefix` set to its prefix and describes a durable it owns, or publishes to a subject it may publish to and awaits the acknowledgement
- **THEN** the request completes

### Requirement: Each deletion producer has its own acknowledgement durable

Edge SHALL create the durables `ratatoskr_chatgpt_deletion_acks`, `ratatoskr_claude_deletion_acks`, `ratatoskr_x_deletion_acks`, `ratatoskr_instagram_deletion_acks` and `ratatoskr_threads_deletion_acks` on `ratatoskr_events`, each with filter `evt.knowledge.deletion.acknowledged.v1`, explicit acknowledgements, deliver all, a 30 second ack wait and unlimited redelivery. The deployed ACL SHALL let KNOWLEDGE publish that subject and SHALL let each of CHATGPT, CLAUDE, X, INSTAGRAM and THREADS inspect, pull from and acknowledge its own durable and no other.

#### Scenario: a fresh broker gets the five durables

- **WHEN** Edge provisions the topology on a broker holding only the two streams
- **THEN** the five durables exist with exactly the stated settings

#### Scenario: a producer reads its own durable and no sibling's

- **WHEN** the CHATGPT identity describes, fetches from and acknowledges `ratatoskr_chatgpt_deletion_acks` and then describes `ratatoskr_claude_deletion_acks` on a broker running the deployed file
- **THEN** the first three succeed and the last request is not answered

#### Scenario: only Knowledge can publish the acknowledgement

- **WHEN** any identity other than KNOWLEDGE publishes `evt.knowledge.deletion.acknowledged.v1` on a broker running the deployed file
- **THEN** the publish is not acknowledged

### Requirement: Edge erases a producer's own stream messages on request

Edge SHALL consume `cmd.platform.stream_erasure.requested.v1` through the durable `platform_stream_erasure` and, for each sequence the command names, SHALL erase the stored message from `ratatoskr_events` only when its subject is one of the seven content-bearing `evt.ai_archive.*` subjects (not `evt.ai_archive.subject.tombstoned.v1`), its envelope `producer` equals the command's producer and its envelope `tenant_id` equals the command's `owner`. A command whose producer is not `ratatoskr-chatgpt` or `ratatoskr-claude`, whose `tenant_id` differs from `payload.owner` or whose `aggregate_id` differs from `payload.request_ref` SHALL be Termed. Edge SHALL write one audit row per command with the counts of erased, missing and refused sequences and no message content, and SHALL count `platform_stream_erasure_total{outcome}`.

#### Scenario: a producer erases its own fact

- **WHEN** the ChatGPT identity publishes a conversation fact for owner O and then a command naming O and that sequence
- **THEN** the stream message is gone, its neighbour is not, and the audit row counts one erased

#### Scenario: another producer's message is refused

- **WHEN** a command from `ratatoskr-claude` names the sequence of a message published by ChatGPT
- **THEN** the message survives and the audit row counts one refused

#### Scenario: another owner's message is refused

- **WHEN** a command naming owner P names the sequence of a message of owner O
- **THEN** the message survives and the sequence is counted refused

#### Scenario: a tombstone is never erased

- **WHEN** a command names the sequence of an `evt.ai_archive.subject.tombstoned.v1` message or of a message outside `evt.ai_archive`
- **THEN** the message survives and the sequence is counted refused

#### Scenario: a replay erases nothing twice

- **WHEN** the same command id is delivered a second time
- **THEN** the inbox absorbs it and no further audit row is written

### Requirement: No stanza but Edge may manage a stream

The deployed ACL SHALL grant CHATGPT and CLAUDE exactly one publish subject for erasure, `cmd.platform.stream_erasure.requested.v1`, in addition to their previous list, and SHALL contain no publish allow starting with `$JS.API.STREAM.` outside the EDGE stanza except the browser worker's read of its own key-value bucket stream info.

#### Scenario: the static ACL keeps stream management with Edge

- **WHEN** the comment-stripped deployed file is parsed
- **THEN** no non-EDGE stanza holds a stream-management publish allow other than `$JS.API.STREAM.INFO.KV_browser_worker_completions`

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
