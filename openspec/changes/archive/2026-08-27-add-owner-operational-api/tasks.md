## 1. Contract pin

- [x] 1.1 Pin every Contracts git dependency and `ratatoskr-operational-contracts` to
  `9a4df8126b495ffc3ad0647441da1690594f25bc`; configuration cannot start from a behavior
  test, so verify `cargo fetch --locked` resolves one git revision and review `Cargo.lock`.

## 2. Live owner capabilities

- [x] 2.1 RED: add `operational_capabilities_follow_live_owner_grant` to
  `crates/public-api/tests/capabilities.rs`; run it and verify the owner-capability assertion fails
  while member, revocation, and dependency-failure expectations compile.
- [x] 2.2 GREEN: add canonical operational capabilities and one bounded live grant lookup; run the
  focused test and public-api crate tests until they pass without session-cached authorization.

## 3. Operation inspection

- [x] 3.1 RED: add `owner_lists_and_reads_cross_user_operations_while_member_is_denied` to
  `crates/public-api/tests/admin_operations.rs`; run it and verify the missing admin route
  assertion fails after covering pagination, filters, redaction, detail, and ordinary-route
  ownership.
- [x] 3.2 GREEN: add bounded repository queries and owner-only operation list/detail routes using
  shared contract responses; run the focused test and operations/public-api crate tests.

## 4. Schedule inspection

- [x] 4.1 RED: add `owner_reads_schedule_status_without_payloads` to
  `crates/public-api/tests/admin_schedules.rs`; run it and verify the missing route assertion fails
  after covering deterministic pagination, never-run, disabled-failed, and redaction states.
- [x] 4.2 GREEN: add a keyset query over `operations.schedule_status` and its owner-only route; run
  the focused test and verify no schema or migration file changed.

## 5. Audit inspection

- [x] 5.1 RED: add `owner_reads_stable_redacted_audit_pages` to
  `crates/public-api/tests/admin_audit.rs`; run it and verify the missing route assertion fails
  after covering member denial, stable ordering, nullable attribution, continuity, and redaction.
- [x] 5.2 GREEN: add the bounded audit query and owner-only route using the shared audit page; run
  the focused test and identity/public-api crate tests.

## 6. Public status

- [x] 6.1 RED: add `public_status_is_anonymous_degraded_and_sanitized` to
  `crates/public-api/tests/status.rs`; run it and verify the missing route assertion fails after
  covering credential independence, healthy, stale/degraded, unknown, unavailable, privacy, and
  `Cache-Control: no-store`.
- [x] 6.2 GREEN: expose only cached RuntimeState/gateway observations and add anonymous
  `GET /v1/status` without request-time I/O; run the focused test and public-api crate tests.

## 7. Generated API and authorization matrix

- [x] 7.1 RED: add `operational_and_status_security_is_exact` to the openapic route/schema tests;
  run it and verify committed OpenAPI drift plus missing public/admin security assertions fail.
- [x] 7.2 GREEN: register routes and shared schemas, regenerate `openapi/openapi.json`, review the
  full generated diff, and rerun the focused openapic tests.
- [x] 7.3 RED: add `every_admin_route_rechecks_owner_and_fails_closed` to
  `crates/public-api/tests/admin_authorization_matrix.rs`; run it and verify any route that does
  not distinguish absent/member/owner/revoked/database-failure states fails the matrix.
  The first matrix run passed because tasks 3.2, 4.2, and 5.2 already routed every handler through
  the shared adapter; the earlier per-route tests supplied the failing owner-gating evidence.
- [x] 7.4 GREEN: consolidate only the shared owner-check adapter needed by the matrix; run the
  matrix and public-api crate tests without changing ordinary ownership semantics.

## 8. Documentation and lifecycle

- [x] 8.1 Update README, DEVELOPMENT, interface, and privacy documentation after behavior exists;
  documentation cannot start from a behavior test, so verify public status, operator health, and
  out-of-band `platform.owner` provisioning are distinguished without real identifiers.
- [x] 8.2 Run focused tests, then the exact DEVELOPMENT gate through `build-gate --`, inspect the
  final diff, validate and archive the OpenSpec change, and verify
  `openspec validate --archived --strict` passes.
