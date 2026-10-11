## ADDED Requirements

### Requirement: The deployed broker accepts envelopes up to 8 MiB

`deploy/nats/ratatoskr.conf` SHALL set `max_payload: 8MB` at the top level so that archive and document facts, which are inline envelopes, can be published. JetStream stream message size limits SHALL stay unchanged.

#### Scenario: the deployed file sets the limit

- **WHEN** the comment-stripped deployed file is read
- **THEN** it holds a top-level `max_payload: 8MB`

#### Scenario: a four mebibyte envelope is acknowledged

- **WHEN** the X identity publishes a 4 MiB envelope on `evt.platform.operation.reported.v1` to a broker running the rendered deployed file
- **THEN** the publish is acknowledged
