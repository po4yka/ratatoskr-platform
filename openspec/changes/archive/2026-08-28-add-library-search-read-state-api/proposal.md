## Why

Clients must call Platform rather than Knowledge's loopback operator surface. Platform currently has no authenticated, documented library API, so Telegram cannot search or update read state without bypassing tenant authorization or inventing a wire contract.

## What Changes

- Add session-authenticated `GET /v1/library/search` with bounded query, `read_state`, limit, and offset parameters and a typed paged response.
- Add idempotent `PUT /v1/library/items/{analysis_id}/read-state` accepting only the complete `read_state` resource.
- Derive the canonical Knowledge tenant from the authenticated internal user; never accept a public tenant selector or forward caller-supplied reserved identity headers.
- Add a dedicated bounded Knowledge client that maps upstream responses into public types and stable Platform errors without exposing internal topology or raw errors.
- Add `library.search` and `library.read_state` to capability discovery only while the database/session path and last observed Knowledge dependency health are available.
- Generate the first-version OpenAPI document from the same route table and types as the handlers.
- Keep domain search/ranking, read-state persistence, bulk actions, favorites, tags/collections, and asynchronous Platform operations outside this change.
- Conform to workspace change `add-library-search-read-state-contract`; consume Knowledge only after `expose-library-search-read-state` is available.

## Capabilities

### New Capabilities

- `library-api`: Authenticated public search/browse and read-state resource operations plus capability discovery and safe Knowledge delegation.

### Modified Capabilities

- None.

## Impact

- `crates/public-api` gains route handlers, schemas, upstream mapping, authorization tests, and OpenAPI registration.
- `crates/core` gains closed capability/requirement variants for Knowledge-backed library access.
- Edge configuration/readiness reuses the existing bounded single-host Knowledge route and last-observed health; no new public listener or database schema is added.
- Generated `openapi/openapi.json` changes additively and downstream generated clients must be refreshed in their own consumer changes.
- Rollback removes the public routes/capabilities after Telegram is rolled back; Knowledge's additive internal behavior may remain deployed safely.
