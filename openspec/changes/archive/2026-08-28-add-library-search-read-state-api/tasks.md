## 1. Authenticated search façade

- [x] 1.1 RED: add `crates/public-api/tests/library.rs::search_uses_principal_tenant_and_returns_only_public_fields` with an authenticated principal and recording Knowledge harness, run it, and confirm 404/no upstream call because the route does not exist.
- [x] 1.2 GREEN: add the strict public request/response types, dedicated bounded Knowledge client, `GET /v1/library/search` handler, route-table entry, and schema registration; rerun the focused test and verify canonical principal tenant forwarding, public-field minimization, pagination facts, and no-store response.
- [x] 1.3 RED: add `crates/public-api/tests/library.rs::invalid_search_input_and_forged_identity_stop_before_knowledge` covering oversized query, page bounds, unknown state/field, `tenant`, and reserved headers; run it and confirm at least one request reaches the harness or is not mapped to invalid request.
- [x] 1.4 GREEN: enforce exact query deserialization, Unicode/page bounds, reserved-header stripping, and safe validation mapping before delegation; rerun the focused test and verify the Knowledge harness records zero invalid calls.

## 2. Read-state resource and failure mapping

- [x] 2.1 RED: add `crates/public-api/tests/library.rs::read_state_put_is_idempotent_and_hides_foreign_targets` asserting two owner PUTs return `read`, favorite is preserved by the harness, and foreign/missing targets share one envelope; run it and confirm the absent route fails.
- [x] 2.2 GREEN: implement and document `PUT /v1/library/items/{analysis_id}/read-state` through the fixed Knowledge command with strict body/path validation and scoped error mapping; rerun the focused test and verify idempotent authoritative responses.
- [x] 2.3 RED: add `crates/public-api/tests/library.rs::knowledge_timeout_and_invalid_success_map_to_safe_errors` using delayed, oversized, and malformed success responses; run it and confirm the current client cannot produce the required timeout/invalid-upstream envelopes without leaking bodies.
- [x] 2.4 GREEN: enforce connect/header/total deadlines, response-size limit, strict closed-state/finite-score decoding, and class-only diagnostics; rerun the failure tests and verify raw body/topology content is absent.

## 3. Capability discovery

- [x] 3.1 RED: extend `crates/public-api/tests/capabilities.rs::library_capabilities_follow_last_knowledge_observation` to assert both closed names and zero/one metric samples across healthy/stale states, run it, and confirm the names/requirement do not exist.
- [x] 3.2 GREEN: add the closed capability variants, Knowledge-backed deployment requirement, background-observation wiring, document sorting, and metric sampling; rerun capability tests and verify names disappear immediately after the observed state turns unhealthy.

## 4. OpenAPI, documentation, and gate

- [x] 4.1 RED: extend `tools/openapic/tests/document.rs::library_routes_are_session_authenticated_and_schema_complete`, run it against the current generated document, and confirm both route/schema assertions fail.
- [x] 4.2 GREEN: register the library route docs and schemas and regenerate `openapi/openapi.json`; rerun openapic/API drift tests and verify router and document carry identical methods, security, parameters, responses, and error envelopes.
- [x] 4.3 Update README/architecture/capability documentation and cite workspace contract `library-search-read-state`; cannot start from a failing behavior test because this is documentation, so verify with repository documentation checks.
- [x] 4.4 Run the complete `DEVELOPMENT.md` gate plus strict validation of this OpenSpec change and archived/spec state; verify all commands pass with the intended Knowledge revision.
