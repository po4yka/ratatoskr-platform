## Why

The export agent has a truthful operation reader, but no authenticated Platform endpoint can accept
an archive, mint an owned operation, and give its identifier to the provider-owned importer. Direct
agent-to-provider uploads would bypass Platform's device authorization and operation projection.

## What Changes

- Add an authenticated, idempotent AI-archive operation-creation route and a separate operation-bound
  streaming route to Edge. The first returns a durable `ai_archive.import` identifier; the second
  streams bytes only to a configured loopback archive service.
- Forward only Edge-minted owner, device, correlation, and operation-id claims; never forward
  caller authorization, provider credentials, local paths, or archive metadata beyond the bounded
  receipt request.
- Return the durable Platform operation identifier before the archive transfer, so the export agent
  can persist and poll it even if the subsequent upload fails.
- Preserve truthful failure: an upstream refusal or timeout makes that already-known operation
  terminal `failed`, and a request body is streamed under the configured transfer budget.

## Capabilities

### New Capabilities

- `ai-archive-acceptance`: Authenticated, idempotent public acceptance and bounded forwarding of an
  AI archive into its provider-owned importer.

### Modified Capabilities

- `edge-gateway`: Operation-bound archive forwarding adds an Edge-minted operation claim to the
  existing bounded transfer proxy behavior.

## Impact

Touches Platform public API, operation/idempotency/audit persistence, gateway request forwarding,
OpenAPI, deployment configuration, and Edge integration tests. It cites the workspace
`operation-progress` and active `ai-archive-operation-summary` changes: producer services remain
the only owners of parsing and terminal operation reports.
