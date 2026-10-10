## Purpose

Defines the fixed durable consumers and KV bucket that Edge provisions on the Platform-owned streams, and the NATS identity rules of the deployed ACL, so that every service of the fleet can find its durable and none can reach beyond it.

## ADDED Requirements

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
