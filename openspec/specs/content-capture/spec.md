# content-capture Specification

## Purpose
Defines how Platform accepts a request to capture content and hands it to the extractor as a typed contract command.

## Requirements

### Requirement: Generic captures are typed command envelopes

Platform SHALL store, for every accepted URL capture from the public route and from the webhook adapter, a complete `CommandEnvelope` whose payload is `ContentCaptureRequested` in its URL form, produced by `ratatoskr-platform`, with aggregate `operation:<operation id>`, tenant `user:<principal>` where the capture has a principal, and an idempotency key equal to the sha256 of the caller's idempotency string.

#### Scenario: the outbox row decodes as the contract command

- **WHEN** a session posts a URL capture
- **THEN** the stored outbox payload decodes with the contract envelope parser, its payload decodes as `ContentCaptureRequested` with the URL form, and its producer, aggregate and tenant are the specified values

### Requirement: Telegram sessions can capture a stored blob

Platform SHALL accept `POST /v1/captures/blobs` only from a Telegram Mini App session, only for a sha256 blob owned by `ratatoskr-telegram` whose length is between 1 and 52428800 bytes, SHALL require an idempotency key, SHALL answer 202 with the operation id, and SHALL emit exactly one `cmd.content.capture.requested.v1` carrying the blob form.

#### Scenario: a Telegram blob capture is accepted once

- **WHEN** a Telegram session posts a valid blob with an idempotency key, and retries with the same key and blob
- **THEN** both responses carry the same operation id and exactly one command was emitted

#### Scenario: other session kinds and foreign owners are refused

- **WHEN** a browser, device or API token session posts a blob, or a Telegram session names the owner `ratatoskr-vault`
- **THEN** the response is 403 `platform.auth.forbidden` and no operation or outbox row exists

#### Scenario: the same key with a different blob conflicts

- **WHEN** the same idempotency key is reused with a different blob
- **THEN** the response is 409
