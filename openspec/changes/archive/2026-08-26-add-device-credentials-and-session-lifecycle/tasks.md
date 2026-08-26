## 1. Decision and contract

- [x] 1.1 Add ADR-0016, the ADR index entry, and implementation-status documentation; documentation
  cannot start RED, so verify their ADR links and stated API model by review.
- [x] 1.2 Add the device-credential API types and OpenAPI route declarations; verify
  `cargo run -p openapic -- generate` changes only the expected `openapi/openapi.json` artifact.
- [x] 1.3 Create the required workspace changeset in a separate clean workspace worktree, listing
  Platform as producer; mobile, browser-extension, export-agent, and web as consumers; the
  rollout order; and rollback behavior. Verified
  `coordinate-device-credentials-lifecycle` with the workspace OpenSpec validator.

## 2. Pairing approval and redemption

- [x] 2.1 Add identity coverage for live-code redemption, expiry/supersession/attestation refusal,
  and `five_mismatched_attestations_permanently_burn_a_pairing_code`; the five-attempt assertion
  was observed RED before the durable attempt budget was implemented.
- [x] 2.2 Implement pairing-code schema and identity persistence; verify
  `cargo test -p ratatoskr-platform-identity --test pairing --locked` passes with digest-only,
  expiry, single-use, and five-attempt behavior.
- [x] 2.3 Add public coverage for authenticated code creation, uniform pairing refusal, and
  `a_device_session_cannot_create_a_pairing_code`; the primary-session denial was observed RED
  before the handler restriction was implemented.
- [x] 2.4 Implement pairing-code and public-pair handlers, with primary-session enforcement,
  atomic device/session/first-refresh creation, uniform denial, and audit recording; verify
  `cargo test -p ratatoskr-platform-public-api --test devices --locked` passes.

## 3. Refresh and lifecycle persistence

- [x] 3.1 Add `credential_rotation_swaps_access_and_replay_burns_the_family` coverage for atomic
  access/refresh rotation and replay containment.
- [x] 3.2 Implement refresh issuance, atomic rotation, replay containment, and access-token rotation;
  verify `cargo test -p ratatoskr-platform-identity --test lifecycle --locked` passes.
- [x] 3.3 Add identity coverage for active session/device listing, liveness, and
  `device_revocation_answers_with_the_sessions_it_kills` atomic cascade behavior.
- [x] 3.4 Implement owner-scoped active lists and atomic device/revoke-all persistence; verify the
  identity lifecycle test suite passes.

## 4. Public lifecycle API

- [x] 4.1 Add public tests for refresh rotation/replay, session/device tenant isolation, individual
  revocation, device cascade, audit grant/denial paths, and complete revoke-all.
- [x] 4.2 Implement device-session opening and refresh plus session list/delete/revoke-all and device
  list/delete routes with owner-scoped queries and audit records; verify the public lifecycle suites
  pass.
- [x] 4.3 Add public-route OpenAPI and artifact-drift assertions for every lifecycle route.
- [x] 4.4 Generate `openapi/openapi.json` from the implemented route table and verify the OpenAPI
  tests pass without weakening the drift check.

## 5. Full verification and delivery

- [x] 5.1 Run the repository gate through the machine build gate — `cargo deny check`, `cargo fmt
  --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `build-gate -- cargo build --workspace --locked`, `build-gate -- cargo test --workspace --locked`,
  and `build-gate -- cargo build --workspace --locked --release --jobs 2` — and resolve the
  in-scope exhaustive-match and lint failures caused by the new HTTP method and tests.
- [x] 5.2 Run `npx --yes --package=@fission-ai/openspec@1.10.0 openspec validate --all --strict` and
  `... validate --archived`; archive only after every task is ticked and the full gate passes.
