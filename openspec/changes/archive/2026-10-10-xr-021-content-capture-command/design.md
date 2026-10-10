## Context

See [proposal.md](proposal.md) and XR-021 CONTRACTS.md section S11. The reconciliation in that section decided that the typed `CommandEnvelope` wins over canonicalizing only the payload of the legacy document.

## Decisions

- **Mirror the social lane.** `captures.rs` already builds a `CommandEnvelope` for `SocialCaptureRequested`; the generic branch uses the same construction with `ContentCaptureRequested`, producer `ratatoskr-platform`, aggregate `operation:<id>`, tenant `user:<principal>` and the sha256 of the caller's idempotency string as `idempotency_key`.
- **`accept` takes the built payload and the route string.** The blob route shares the reserve, operation, outbox, ledger and audit transaction with the URL route instead of copying it.
- **Authorization is the only guard on the blob owner.** The store is content-addressed and deduplicated across users, so the gate against naming a foreign owner's bytes is the Telegram-only session kind, the `ratatoskr-telegram` owner allowlist and the unguessable 256-bit digest. This residual risk is documented.
- **No change to `SubmitCapture`.** The generated web, mobile and browser-extension clients pin the existing operation; the new route is additive.

## Risks / Trade-offs

- Platform does not judge whether a media type is extractable; the extractor reports `unsupported_media` through the normal operation report.
