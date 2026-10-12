## ADDED Requirements

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
