## 0. Pin and open the changes

- [x] 0.1 Move every `ratatoskr-*` contracts dependency in the root `Cargo.toml` to `ad16855c4e7f3d52cd118274faa3b8f3ab4da576`, add the AI-archive, document and channel-digest contract crates, refresh `Cargo.lock`, and run `cargo test --workspace --locked` and `cargo run --locked -p openapic -- check`. This is a dependency pin and cannot start from a failing test; the existing suite staying green is the gate.

## 1. The profile crate

- [x] 1.1 Add `crates/nats-profile/tests/profile.rs` with `parse_reads_every_stanza_of_a_fixture_conf`, `render_replaces_every_placeholder_with_a_valid_nkey_and_returns_matching_seeds` and `render_output_is_accepted_by_nats_server_dash_t` against stub functions that return empty values, and run them RED on their assertions
- [x] 1.2 Implement `parse`, `render` and the `render-nats-config` binary and run the three tests GREEN

## 2. The fixed table

- [x] 2.1 Add `crates/eventing/tests/fixed_topology.rs::ensure_domain_topology_creates_every_fixed_consumer_and_the_kv_bucket` against an `ensure_domain_topology` that returns `Ok(())` over an empty table and run it RED on the missing durable
- [x] 2.2 Extend `FixedConsumerSpec`, add `DOMAIN_CONSUMERS` and the KV bucket, generalise the ensure function, fold the old wrappers into it, update every caller and run the test GREEN

## 3. Edge provisions the topology

- [x] 3.1 Add the `services/edge/tests/boot.rs` assertion that every durable and the KV bucket exist once Edge reports ready and run it RED
- [x] 3.2 Call the single provisioning function from `ensure_fixed_bus_topology` and run the boot test GREEN

## 4. The deployed ACL

- [x] 4.1 Add `deployed_nats_config.rs::the_deployed_config_is_reachable_through_a_published_port` and run it RED against the container-loopback bind
- [x] 4.2 Add `deployment_profile.rs` tests for the thirteen identities, exclusive durable ownership and the S03 invariants (replacing the older social-identity test) and run them RED
- [x] 4.3 Rewrite `ratatoskr.conf` per S03, extend `deploy/nats/README.md` and `deploy/README.md`, and run 4.1 and 4.2 GREEN

## 5. The permission matrix

- [x] 5.1 Add the per-identity real-broker matrix and the generic cross-durable refusal loop in `crates/eventing/tests/deployed_nats_config.rs` with shared helpers in `tests/support/mod.rs`, and run it RED on the identities that do not exist
- [x] 5.2 Make the matrix GREEN, adding exactly the missing KV subject for the browser worker if the broker shows one

## 6. CI smoke

- [x] 6.1 Add `deployment_profile.rs::ci_smoke_uses_the_rendered_deployed_nats_config` and run it RED against the unauthenticated smoke broker
- [x] 6.2 Render the deployed config in the smoke step, start the smoke broker with it, give Edge the Edge seed and run the test GREEN
