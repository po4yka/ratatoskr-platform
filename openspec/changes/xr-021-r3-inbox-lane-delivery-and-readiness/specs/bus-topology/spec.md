## ADDED Requirements

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
