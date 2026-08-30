## 1. Fixed GitHub durables

- [x] 1.1 RED — add `github_durables_have_exact_stream_filters_and_delivery_limits` to `crates/eventing/tests/stream_limits.rs`; run `build-gate -- cargo test -p ratatoskr-platform-eventing --test stream_limits --locked github_durables_have_exact_stream_filters_and_delivery_limits` and observe the missing-consumer assertion.
- [x] 1.2 GREEN — add the four fixed consumer specifications and idempotent drift-refusing provisioning path; rerun the targeted test until green.

## 2. Least-privilege GitHub identity

- [x] 2.1 RED — add `github_identity_can_use_only_declared_bus_paths` to `crates/eventing/tests/nats_permissions.rs`; run the locked real-broker test through `build-gate --` and observe the missing-identity assertion.
- [x] 2.2 GREEN — add the GitHub public-key placeholder and exact permissions, extend transient synthetic key generation and `deploy/nats/README.md`, then rerun the permission test and complete `DEVELOPMENT.md` gate.

## 3. Verification

- [x] 3.1 Run `openspec validate activate-github-fleet-bus-runtime --strict`, inspect the complete diff for scope and secrets, commit only GHB-017 paths, and publish the authorized branch with exact local/remote SHA verification.
