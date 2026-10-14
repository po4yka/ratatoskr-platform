## MODIFIED Requirements

### Requirement: The deployed ACL grants each service only its own durables

`deploy/nats/ratatoskr.conf` SHALL define exactly fourteen identities, grant `$JS.API.>`, `cmd.>` and undurable `$JS.ACK.>` only to Edge, allow no identity other than Edge to create or update consumers or streams or to read stream messages directly, and grant each fixed durable's inspect, fetch and acknowledge subjects to exactly one owner identity.

#### Scenario: an identity cannot inspect a durable it does not own

- **WHEN** any non-Edge identity requests consumer info for a fixed durable owned by another identity on a real broker running the deployed file
- **THEN** the request is not answered

#### Scenario: the deployed file is reachable through a published port

- **WHEN** the rendered deployed file runs in a container whose client port is published to the host
- **THEN** a client holding the Edge seed completes a JetStream account-info request within ten seconds

### Requirement: Each non-Edge identity subscribes only to its own inbox prefix

`deploy/nats/ratatoskr.conf` SHALL give each of the thirteen non-Edge identities `subscribe: { allow: ["_INBOX_<name>.>"] }` where `<name>` is the identity name in lower case with its underscores kept, and nothing else. Edge SHALL keep `_INBOX.>`. A service SHALL connect with `custom_inbox_prefix` set to its own prefix.

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

## ADDED Requirements

### Requirement: The NOTES identity publishes only the note events and the report subject

The deployed ACL SHALL hold the identity `NOTES`, which publishes `evt.notes.vault.connected.v1`, `evt.notes.vault.disconnected.v1`, `evt.notes.file.changed.v1` and `evt.platform.operation.reported.v1`, subscribes only to `_INBOX_notes.>` and owns no durable.

#### Scenario: Notes publishes a file change

- **WHEN** `NOTES` publishes `evt.notes.file.changed.v1` on a broker running the rendered deployed file
- **THEN** the publish is acknowledged, verified by `crates/eventing/tests/nats_permissions.rs::notes_identity_publishes_its_events_and_knowledge_reads_its_durables`

### Requirement: Knowledge reads note events from two durables

Edge SHALL create `ratatoskr_knowledge_notes_files` (filter `evt.notes.file.changed.v1`) and `ratatoskr_knowledge_notes_vaults` (filter `evt.notes.vault.>`) on `ratatoskr_events` with explicit acknowledgements, deliver all, a 120 second ack wait and unlimited delivery. The ACL SHALL grant their inspect, fetch and acknowledge subjects to `KNOWLEDGE` only.

#### Scenario: Notes publishes and Knowledge reads

- **WHEN** Edge provisions the topology on a broker running the rendered deployed file, `NOTES` publishes `evt.notes.file.changed.v1` and `evt.notes.vault.connected.v1`, and `KNOWLEDGE` fetches from both durables
- **THEN** each publish is acknowledged and each durable delivers its message to `KNOWLEDGE`, verified by `crates/eventing/tests/nats_permissions.rs::notes_identity_publishes_its_events_and_knowledge_reads_its_durables`
