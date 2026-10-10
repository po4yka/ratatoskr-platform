## Why

`POST /v1/captures` and the webhook adapter emit `content.capture.requested.v1` as Platform's legacy command document, which the extractor can no longer decode once the typed `CommandEnvelope` form is the contract, and the Telegram service has no route through which a PDF it stored can be captured. The social capture reports the browser extension reads also have no Platform-side proof.

Cites XR-021 CONTRACTS.md section S11 (the typed command and the blob route) and CD1 and CD2 of section S10 (report projection).

## What Changes

- The non-social branch of `POST /v1/captures` and the ingest webhook adapter build a typed `CommandEnvelope` carrying `ContentCaptureRequested` (url form) instead of the legacy `platform_eventing::Command`. The legacy `Command` struct stays: the scheduler and the cancel path still use it.
- New `POST /v1/captures/blobs` (`submitBlobCapture`): Telegram Mini App sessions only, naming only `ratatoskr-telegram` blobs of 1 byte to 50 MiB, emitting the blob form of the same command.
- A characterization test proves Platform already projects the social capture reports with the codes and result references the browser extension reads.

## Capabilities

### New Capabilities

- `content-capture`: the typed capture command and the blob capture route.

## Impact

- `crates/public-api` (captures, lib, tests), `crates/ingest`, `crates/eventing` tests, `openapi/openapi.json` (regenerated), `crates/public-api/Cargo.toml`.
- Deliberate break: the outbox row for a generic capture changes shape (a complete `CommandEnvelope`), so interleaved old and new Platform and extractor binaries drop capture commands. Platform, extractor and X deploy together (XR-021 S14).
