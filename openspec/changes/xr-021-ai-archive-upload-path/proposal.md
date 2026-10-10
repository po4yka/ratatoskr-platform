## Why

The AI-archive path from the export agent through Edge to the ChatGPT and Claude receivers has six independent defects that stop an archive from ever reaching `succeeded`: the chunk route rejects chunks above axum's 2 MiB default although the contract allows 16 MiB, a non-zip media type is accepted at open, the archive size ceiling is the transfer body budget and answers 400 instead of 413, the receipt forward uses PUT without a content type and reads the whole archive into memory, the receiver's readiness only checks that a probe is fresh, and an incomplete import report poisons the stored snapshot so GET answers 504.

Cites XR-021 CONTRACTS.md section S06 (decisions D1 to D6).

## What Changes

- D4: `DefaultBodyLimit::max(CHUNK_SIZE_MAX_BYTES)` on the chunk route only.
- D6: refuse a non-`application/zip` media type when a transfer is opened.
- D5: new `archive_staging.max_archive_bytes` (default 2 GiB, range 1 MiB to 10 GiB, rule V21) carried into `ApiState.archive_max_bytes`; oversize answers 413 `payload_too_large`.
- D1: the receipt forward becomes `POST` with `Content-Type: application/zip`, streamed from the verified staging file in 64 KiB frames, with the staging directory removed after success.
- D2: archive readiness requires the receiver's receipt capability document, and ADR-0015 records that the capability probe is the single route exempt from minted claims.
- D3: `ProgressReport::read` validates the report and an invalid one takes the existing rejection path instead of being applied.

## Capabilities

### Modified Capabilities

- `ai-archive-acceptance`: the upload and finalize behaviour described above.

## Impact

- `crates/public-api` (archives, gateway, capabilities), `crates/core` (config model and validation), `crates/operations/src/projection.rs`, `services/edge/src/main.rs`, `openapi/openapi.json` (regenerated), `.env.example`, `deploy/systemd/edge.conf.example`, `DEVELOPMENT.md`, `README.md`, `docs/adr/0015-edge-routing-model.md`.
- Deliberate breaks: the receipt forward changes from PUT to POST (both receivers already answer POST), and the archive ceiling no longer follows `public.max_body_bytes`.
