## 1. Public archive acceptance

- [x] 1.1 RED: add `crates/public-api/tests/ai_archives.rs::configured_device_archive_preparation_creates_one_owned_operation`; assert a valid device request receives `202`, an operation id, and an operation-bound upload path, and observe the current route is absent.
- [x] 1.2 GREEN: add the authenticated, idempotent archive-preparation route, response DTO/OpenAPI entry, operation and audit persistence; rerun the new integration test and replay/unknown-provider cases.

## 2. Operation-bound streaming gateway

- [x] 2.1 RED: extend `crates/public-api/tests/ai_archives.rs` with `upload_forwards_only_edge_minted_archive_claims_to_the_fixed_receipt`; assert the operation-bound upload route makes the configured loopback receiver see only Edge-minted claims and the prepared operation id.
- [x] 2.2 GREEN: add the operation-bound streaming route, extend the bounded transfer gateway to forward the minted operation claim, remove caller credentials, and record a safe failed operation on refusal or timeout; rerun the forwarding, transfer-limit, and refusal tests.

## 3. Configuration and contract evidence

- [x] 3.1 Add archive provider/receiver configuration validation and generated OpenAPI/deployment artifacts; this configuration/generated-artifact task has no failing unit test. Verify invalid provider configuration refuses startup and the generated document is current.
- [x] 3.2 Run the Platform full gate after both RED/GREEN pairs: fetch, deny, fmt, clippy, size, debug build, workspace test, release build, OpenSpec validations, and `git diff --check`.

## 4. Coordinated handoff

- [ ] 4.1 Record the acceptance route and minted operation claim in the workspace changeset, then add producer RED/GREEN task pairs that require their receipt paths to emit a terminal `platform.operation.reported.v1`; verify each producer has its own active OpenSpec change.
