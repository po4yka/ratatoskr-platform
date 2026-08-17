# Platform testing strategy

Required suites:

- Session creation/rotation/expiry/revocation and device pairing.
- Authorization matrices and non-disclosure of unauthorized resources.
- Idempotent concurrent request replay.
- Operation state, partial success, cancellation, stale/duplicate events.
- Transactional outbox/inbox, redelivery, and dead-letter behavior.
- OAuth callback and Telegram assertion validation.
- REST/OpenAPI/generated-client compatibility.
- Rate/body/upload limits, audit, health, and readiness.
- Database migrations with old/new binary compatibility.
- Workspace capture flow through one real domain service.

Tests use synthetic identities and local dependencies; no production provider tokens.
