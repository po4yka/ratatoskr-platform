## 1. Operation-bound archive transfer

- [x] 1.1 RED — in `crates/public-api/tests/ai_archives.rs`, add
  `operation_bound_upload_resumes_missing_chunks_without_second_operation`; interrupt after
  acknowledged chunks, recreate API state, query status, and assert only missing chunks complete
  the original operation. Run the focused test through `build-gate -- cargo test --locked` and
  observe failure because the operation-scoped open/chunk/status/finalize routes are absent.
- [x] 1.2 GREEN — edit `schema.sql` and the public API/OpenAPI in place to persist a bounded
  operation-owned staging session and implement open/chunk/status/finalize with the exact pinned
  blob-transfer types. Rerun 1.1 and verify restart resumes the same operation.
- [x] 1.3 RED — in `crates/public-api/tests/ai_archives.rs`, add
  `operation_transfer_is_idempotent_and_owner_device_provider_scoped`; assert identical prepare and
  chunk replay return original state, divergent chunks conflict, valid foreign/wrong-device or
  provider requests return bounded 404, and revoked credentials receive common authentication
  rejection without changing chunks. Run the focused test and observe the current path fail.
- [x] 1.4 GREEN — bind preparation and every transfer access to owner, active export-agent device,
  provider and operation; implement bounded expiry/replacement inside the original operation, then
  rerun 1.3 and verify all authority and replay assertions pass.
- [x] 1.5 RED — in `crates/public-api/tests/ai_archives.rs`, add
  `archive_finalization_verifies_before_bound_provider_delivery`; assert ordered assembly, no
  upstream call on digest mismatch, and verified bytes reach only the bound provider with minted
  operation/declaration headers. Run the focused test and observe finalization is absent.
- [x] 1.6 GREEN — implement crash-safe chunk publication, ordered streaming verification,
  fixed-route provider delivery, retry-safe uncertain-outcome state and operation-safe cleanup;
  rerun 1.5 and verify it passes.

## 2. Deployment, secured bus and readiness

- [x] 2.1 RED — extend `crates/core/tests/config_validation.rs` and
  `services/edge/tests/deployment_profile.rs` to require ChatGPT `127.0.0.1:8096` and Claude
  `127.0.0.1:8097` receipt routes and reject collisions or missing routes. Run both focused tests
  and observe `deploy/systemd/edge.conf.example` fail the deployment contract.
- [x] 2.2 GREEN — add the two fixed gateway routes, durable staging paths and required unit/example
  configuration; regenerate checked-in config/OpenAPI artifacts, rerun 2.1 and verify both pass.
- [x] 2.3 RED — extend `crates/eventing/tests/nats_permissions.rs` and the operation projection tests
  so distinct ChatGPT and Claude credentials can publish only their own
  `evt.ai-archive.<provider>.operation.reported.v1` subject, cannot impersonate each other or
  subscribe to `evt.>`, anonymous publication is refused, and a broker-accepted
  subject/producer/bound-provider mismatch cannot advance an operation. Run the focused tests and
  observe the current NATS configuration/projection fail.
- [x] 2.4 GREEN — define least-privilege provider users and credential-file settings, provision two
  fixed durable consumers for the unchanged report envelope, and validate subject, producer and
  operation-bound provider before projection. Rerun 2.3 and verify it passes without committed
  credential material or anonymous fallback.
- [x] 2.5 RED — extend admin readiness and archive route tests to stop one provider receiver and one
  provider report consumer independently; assert the existing admin projection reports the failed
  dependency, only the affected archive preparation returns bounded 404, and no provider remains
  ready without its report path. Run the focused tests and observe current readiness fail.
- [x] 2.6 GREEN — register staging, provider receipt and provider report-consumer health in the
  existing admin readiness projection and consult the same state for route refusal, without adding
  a public capability token. Rerun 2.5 and verify it passes.

## 3. Repository verification

- [x] 3.1 Run every command in `DEVELOPMENT.md` through the machine-wide gate where compiler-backed,
  then run strict current/archived OpenSpec validation and `git diff --check`; record only observed
  results and leave externally dependent deployment evidence open.
