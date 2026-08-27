## Purpose

Accepts a user-owned AI archive through Edge, binds it to one durable Platform operation, and
streams it only to the configured provider-owned archive receiver.

## ADDED Requirements

### Requirement: Device-authenticated archive preparation creates one owned operation

The public API SHALL prepare a provider-selected archive from an authenticated registered device only
when it has a valid idempotency key and a configured transfer receiver. It SHALL create one owned
`ai_archive.import` operation, return its identifier and operation-bound upload path before archive
bytes are sent, and return the same preparation for an idempotent replay. It SHALL not create an
operation when the provider is unsupported.

#### Scenario: A configured archive preparation returns an operation-bound upload path
- **WHEN** an authenticated device prepares a ChatGPT or Claude archive with a fresh idempotency key
- **THEN** Edge returns a new `ai_archive.import` operation identifier and the path that accepts
  bytes only for that operation

#### Scenario: An idempotent retry names the original operation
- **WHEN** the same authenticated device repeats the same archive request and idempotency key
- **THEN** the response names the original operation and no second operation exists

#### Scenario: An unknown provider writes nothing
- **WHEN** an authenticated device names a provider without a configured archive receiver
- **THEN** Edge refuses the request and creates neither an operation nor an outbound transfer

### Requirement: Archive bytes remain provider-owned and bounded in transit

Edge SHALL stream archive request bytes under the configured transfer limit and SHALL NOT persist,
inspect, or buffer archive content. It SHALL retain only the prepared provider, SHA-256 and byte
size as an immutable operation receipt binding. It SHALL remove caller authorization and all
caller-supplied Ratatoskr headers before forwarding, and it SHALL add only minted owner, device,
correlation, operation-id, SHA-256, and byte-size claims. It SHALL stream only to the receiver
matching the prepared provider and owned operation.

#### Scenario: A forwarded receipt receives only minted claims
- **WHEN** a device sends an archive request containing forged identity or operation headers
- **THEN** the provider receiver observes only the Edge-minted claims for the authenticated device
  and the newly accepted operation

#### Scenario: A transfer exceeds its configured budget
- **WHEN** an archive request body exceeds the configured transfer maximum
- **THEN** Edge refuses excess bytes and the provider receiver does not receive them

### Requirement: Downstream failure is reflected truthfully

If a provider receiver refuses the request or cannot return response headers within its configured
transfer timeout, Edge SHALL transition the already-prepared operation to `failed` with a safe
delivery error and return the corresponding public failure envelope. A receiver acceptance leaves
the operation observable for its producer to advance through `platform.operation.reported.v1`.

#### Scenario: A refused receiver makes the prepared operation failed
- **WHEN** the configured provider receiver refuses an archive stream
- **THEN** Edge answers with the public upstream failure and the prepared operation becomes failed
  with a safe delivery error

#### Scenario: A stored archive can later report its terminal result
- **WHEN** the provider receiver accepts a streamed archive and later publishes its terminal
  operation report
- **THEN** the owner can read the same Platform operation identifier and its resulting summary
