# Platform interfaces

## Inbound

Public REST from web/mobile/extension/export-agent; anonymous cached status reads; owner-authorized
operational inspection; OAuth callbacks; validated Telegram identity assertions; generic
webhooks/captures; domain operation events.

## Outbound

REST/SSE/capability/status responses; redacted operation, schedule, and audit pages; typed NATS
commands; session/device events; scheduled commands; audit records.

## Rules

- Public routes use generated OpenAPI contracts and stable error envelopes.
- `Idempotency-Key` is required for mutation/capture APIs where replay is possible.
- Commands include principal, operation, correlation, causation, idempotency, and schema version.
- Events are past-tense facts and consumers use inbox deduplication.
- Assertions validate issuer, signature, audience, expiry, nonce, and subject binding.
- Pagination, cancellation, optimistic concurrency, upload limits, and partial success are explicit.
- Internal provider/database errors are mapped to safe public codes.
- `/v1/status` reads cached observations only, requires no credential, sets `Cache-Control:
  no-store`, and never exposes operator-health detail or deployment topology.
- Every `/v1/admin/*` request requires a session and re-checks the live `platform.owner` grant.
  Client-side route hiding is presentation, not enforcement.
- Operational pages are bounded by a maximum of 100 rows and select only the fields in the shared
  response contract; their queries never load payloads, configuration, credentials, bodies, or
  diagnostics.
