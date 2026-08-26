## Why

Ratatoskr has no device credential model even though registered-device, session, refresh-token, and
audit storage already exists. Mobile, the browser extension, the export agent, and web settings
cannot safely enroll a device, refresh its access, list active login state, or revoke it.

## What Changes

- Add ADR-0016, defining explicit primary-session approval, short-lived single-use pairing codes,
  per-device rotating refresh credentials, durable abuse limits, revocation semantics, and deferred
  per-device scopes.
- Add pairing-code creation, public pairing, and refresh endpoints.
- Add owner-scoped active-session and active-device listing plus individual/revoke-all/session and
  device revocation endpoints.
- Make device deletion atomically revoke its bound sessions and make their refresh credentials
  unusable.
- Extend identity persistence, audit records, route/OpenAPI registration, generated OpenAPI, and
  drift coverage for this public contract.

## Capabilities

### New Capabilities

- `device-credentials`: User-approved device pairing, rotating credentials, owner-scoped lifecycle
  visibility, and revocation.

### Modified Capabilities

- None.

## Impact

- `schema.sql`, `crates/identity`, `crates/public-api`, `crates/api-doc`, and generated
  `openapi/openapi.json`.
- New `/v1/devices/*` and `/v1/sessions/*` public contract routes consumed by the mobile,
  browser-extension, export-agent, and web settings clients.
- ADR index and `DEVELOPMENT.md` change from an explicitly absent device model to implemented
  behavior.
