## Why

Platform currently accepts AI archive bytes through an incomplete whole-file path that cannot resume
inside the original operation, and its deployed gateway, secured bus, and readiness projection do not
make the ChatGPT and Claude receipt/report path usable by Export Agent.

## What Changes

- **BREAKING** Replace the development-only whole-file upload with operation-bound prepare, open,
  chunk, status, and finalize routes using the existing blob-transfer documents.
- Persist bounded staging declarations and chunk state in the current schema, scoped to owner,
  active export-agent device, provider, and operation.
- Verify complete bytes before forwarding them to a fixed provider receipt route and keep provider
  acceptance distinct from terminal import success.
- Configure distinct ChatGPT and Claude loopback routes and least-privilege secured-bus identities.
- Project archive capability/readiness only while staging, provider receipt, and terminal-report
  consumption are healthy.

## Capabilities

### New Capabilities

- `ai-archive-operation-transfer`: Authenticated operation-owned resumable staging, verification,
  and fixed provider delivery.

### Modified Capabilities

- `device-credentials`: Archive transfer requests require an active owner-bound export-agent device.
- `edge-gateway`: The single-host gateway exposes distinct fixed ChatGPT and Claude receipt routes.
- `operation-report-projection`: Provider terminal reports are consumed durably and gate archive
  capability readiness.

## Impact

This changes `schema.sql`, public API/OpenAPI, Edge routing and capability/readiness code, NATS
permissions, deployment examples/units, and their PostgreSQL/NATS integration tests. It changes the
first version in place, adds no migration or second route family, and stores no provider credential
or provider-private archive state.
