## Purpose

Defines the Platform routes through which a user manages channel-digest subscriptions, requests digest runs and reads their results.

## ADDED Requirements

### Requirement: Subscriptions and runs are accepted as idempotent operations

Platform SHALL accept `PUT /v1/channel-digests/subscriptions/{channel_username}` and `POST /v1/channel-digests/runs` with a required `Idempotency-Key`, answer 202 with the operation id, and write in one transaction the operation, one contract command in the outbox, the idempotency record and the audit record. A retry with the same key and body SHALL return the original operation without a second outbox row, and the same key with a different body SHALL answer 409.

#### Scenario: a subscription is accepted and enqueued

- **WHEN** a session puts an active subscription for a valid channel username
- **THEN** the response is 202 and one outbox row on subject `cmd.channel_digest.subscription.set_requested.v1` holds a contract envelope that validates for publication

#### Scenario: a run covers the previous 24 hours

- **WHEN** a session posts a digest run request
- **THEN** the enqueued run window is closed-open, 24 hours long, ends at the acceptance instant, and the on-demand trigger carries that same instant

#### Scenario: a non-canonical username is refused

- **WHEN** the path username is uppercase or shorter than five characters
- **THEN** the response is 400 and nothing is stored

### Requirement: Reads are forwarded to channel-digests with the owner identity

Platform SHALL serve subscriptions, results and one result by calling the channel-digests API on its configured loopback listener with the service secret and the caller's user id, SHALL answer with the contract view types and `Cache-Control: no-store`, and SHALL map an upstream 404 to 404, a transport failure to unavailable or timeout, and any other failure or an invalid or oversize body to an invalid-upstream-response error.

#### Scenario: an absent configuration does not break commands

- **WHEN** the channel-digests listener is not configured
- **THEN** read routes answer upstream-unavailable and the command routes still accept requests

### Requirement: The channel-digests secret is configured with its listener and redacted

Platform SHALL require the channel-digests listener to be a loopback address on port 8098 and SHALL require the listener and the secret to be present together, and SHALL never print the secret.

#### Scenario: configuration with only one of the two keys is refused

- **WHEN** only the listener or only the secret is configured
- **THEN** configuration validation reports a violation naming the section
