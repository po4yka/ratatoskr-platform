## 1. Per-service inbox prefixes (R3-01)

- [x] 1.1 Add `deployed_nats_config.rs::every_non_edge_stanza_subscribes_only_to_its_own_inbox_prefix`, `a_service_cannot_subscribe_to_the_shared_inbox_namespace`, `a_service_cannot_subscribe_to_another_services_inbox_prefix` and `each_service_completes_a_jetstream_request_on_its_own_prefix`, update the `deployment_profile.rs` expectations, and run them RED on the `_INBOX.>` entries
- [x] 1.2 Edit the twelve stanzas and comments of `deploy/nats/ratatoskr.conf`, add the prefix table to `deploy/nats/README.md`, update the `nats_permissions.rs` fixtures, and run 1.1 GREEN

## 2. The recorded lane (R3-02)

- [x] 2.1 Add the signature-only `reporter: Option<&str>` parameter to `accept`, pass `None` at every caller, add the tests of `report_authority.rs`, `capture.rs`, `capture_blobs.rs`, `channel_digests.rs`, `webhook.rs`, `publication.rs` and `ai_archives.rs`, extend `every_minted_operation_kind_has_an_authority_row`, and run them RED on the assertions
- [x] 2.2 Add `operations.operations.reporter` to `schema.sql`, write and validate it in `accept`, pass the lane from the four callers, apply it in `projection` and `authority::authorize`, rewrite the `authority.rs` comment and `docs/ARCHITECTURE.md` 11.2, and run 2.1 GREEN

## 3. Upstream 401 and permanent refusal (R3-03)

- [ ] 3.1 Add `gateway.rs::an_upstream_401_is_answered_502`, `an_upstream_407_is_answered_502` and the `ai_archives.rs` and `ai_archives_receipt.rs` finalize tests for 403, 413, 503 and 401, and run them RED
- [ ] 3.2 Map 401 and 407 in `response_from_upstream`, terminate the delivery on a permanent refusal in `finalize_transfer`, document the responses, regenerate `openapi/openapi.json` with `openapic` if the route documentation changed, and run 3.1 GREEN

## 4. Edge readiness and route diagnostics (R3-13)

- [ ] 4.1 Add `admin.rs::a_down_archive_receiver_does_not_make_edge_unready` and `an_unwritable_staging_root_makes_edge_unready`, `boot.rs` shipped-route-table readiness, and `config_archive_and_digests.rs` route-named violation tests, and run them RED
- [ ] 4.2 Make `is_ready` ignore the four receiver checks, name the route in the violation `key` and `env_var`, and run 4.1 GREEN
- [ ] 4.3 Correct the two `schema.sql` comments about the outbox payload bound. Documentation cannot start from a failing test.

## 5. The assertion key procedure (R3-12)

- [ ] 5.1 Add `deployment_profile.rs::the_edge_example_enables_the_assertion_key` and `identity::the_rfc_8032_vector_verifies`, and run the first RED on the commented-out line (the second documents behaviour that already holds and is not a RED)
- [ ] 5.2 Activate `RATATOSKR__IDENTITY__ASSERTION_KEY=CHANGE-ME` in `edge.conf.example` and `.env.example`, document the procedure in `deploy/README.md`, and run 5.1 GREEN

## 6. Operator documentation (R3-11, R3-19)

- [ ] 6.1 State the PostgreSQL provisioning order in `deploy/README.md` and rewrite the workspace-pin paragraph of `README.md`. Documentation cannot start from a failing test.
