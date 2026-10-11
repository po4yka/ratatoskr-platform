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
