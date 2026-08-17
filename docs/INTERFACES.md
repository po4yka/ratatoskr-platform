# Platform interfaces

## Inbound

Public REST from web/mobile/extension/export-agent; OAuth callbacks; validated Telegram identity assertions; generic webhooks/captures; domain operation events.

## Outbound

REST/SSE/capability responses; typed NATS commands; session/device events; scheduled commands; audit records.

## Rules

- Public routes use generated OpenAPI contracts and stable error envelopes.
- `Idempotency-Key` is required for mutation/capture APIs where replay is possible.
- Commands include principal, operation, correlation, causation, idempotency, and schema version.
- Events are past-tense facts and consumers use inbox deduplication.
- Assertions validate issuer, signature, audience, expiry, nonce, and subject binding.
- Pagination, cancellation, optimistic concurrency, upload limits, and partial success are explicit.
- Internal provider/database errors are mapped to safe public codes.
