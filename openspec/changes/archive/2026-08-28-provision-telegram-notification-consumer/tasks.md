## 1. Fixed Telegram consumer topology

- [x] 1.1 Add `edge_preprovisions_the_telegram_notification_consumer` to `services/edge/tests/boot.rs`, start the current edge against an empty real JetStream server, and verify the test fails because `ratatoskr_telegram_notifications` is absent after startup.
- [x] 1.2 Add the fixed event-consumer specification and edge startup provisioning in `crates/eventing/src/stream.rs` and `services/edge/src/main.rs`; verify `edge_preprovisions_the_telegram_notification_consumer` passes and reports the exact stream, filter, pull mode, and explicit acknowledgement policy.
- [x] 1.3 Add `edge_refuses_a_mismatched_telegram_notification_consumer` to `services/edge/tests/boot.rs`, pre-create the durable with a foreign filter, and verify the test fails because the current edge accepts the mismatch.
- [x] 1.4 Validate every security- and delivery-relevant existing-consumer field and propagate a safe startup error; verify both Telegram consumer boot tests pass without deleting or resetting the pre-created durable.

## 2. Least-privilege NATS identity

- [x] 2.1 Add `telegram_bus_identity_is_limited_to_its_notification_durable` to `services/edge/tests/deployment_profile.rs`, asserting the exact INFO, NEXT, ACK, and inbox grants and the absence of broad JetStream, domain publish, and direct subscription grants; verify it fails because the Telegram stanza is missing.
- [x] 2.2 Add the Telegram public-NKey stanza to `deploy/nats/ratatoskr.conf` with only the exact permissions; verify the deployment-profile test passes.
- [x] 2.3 Add `telegram_nkey_permission_matrix_is_enforced_by_nats` to the eventing real-broker tests, using generated disposable NKeys to assert allowed describe/fetch/ack and denied create/foreign-fetch/domain-publish operations; verify it fails against the pre-change permission fixture for the missing Telegram identity.
- [x] 2.4 Extend the synthetic NATS configuration fixture with the production-equivalent Telegram stanza; verify the permission-matrix test passes without a production seed, chat, or notification payload.

## 3. Deployment documentation and consistency

- [x] 3.1 Extend the deployment-profile test with `telegram_consumer_profile_matches_runtime_constants`, asserting the README names the fixed stream, durable, filter, and `/etc/ratatoskr/telegram.nkey` boundary; verify it fails against the current documentation.
- [x] 3.2 Document Telegram NKey generation/installation/rotation, consumer inspection, provisioning order, and rollback in `deploy/nats/README.md`; verify the structural profile test passes and `rg 'SU[A-Z0-9]{20,}' deploy` finds no seed-like value.
- [x] 3.3 Add unit coverage for the generalized fixed-consumer inventory in `crates/eventing/tests/stream_limits.rs`, including idempotent reuse and mismatch refusal, and verify the new tests fail before the helper is generalized.
- [x] 3.4 Generalize the consumer-provisioning helper without changing existing social consumers, export only the narrow required constants, and verify all eventing and edge tests pass.

## 4. Validation and handoff

- [x] 4.1 Run `cargo fmt --all -- --check`, `cargo test --workspace --locked` with PostgreSQL and JetStream through `build-gate`, and every command in `DEVELOPMENT.md`; record exact green commands and confirm the gate list remains synchronized with CI.
- [x] 4.2 Run `openspec validate provision-telegram-notification-consumer --strict`, review the final diff for credential leakage and unrelated changes, and mark all completed test/implementation pairs only after their failing and passing runs were observed.
