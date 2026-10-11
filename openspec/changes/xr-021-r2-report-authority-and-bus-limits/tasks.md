## 1. Operation report authority (R2-02)

- [x] 1.1 Extend `crates/operations/tests/projection.rs` with the tenant, producer and unknown-kind cases and add `every_minted_operation_kind_has_an_authority_row`, and run them RED on the missing rejections
- [x] 1.2 Add `authority.rs`, load the operation in `ProgressProjection`, apply the table, record the rule and the residual risk in the docs, and run 1.1 GREEN

## 2. Broker payload limit (R2-04 rule 1)

- [x] 2.1 Add `crates/eventing/tests/deployed_max_payload.rs::deployed_conf_sets_max_payload` and `deployed_nats_config.rs::x_identity_publishes_a_four_mebibyte_envelope` and run the static test RED
- [x] 2.2 Set `max_payload: 8MB` in `deploy/nats/ratatoskr.conf`, update the README and `compose.yaml` comments, and run the static test GREEN

## 3. Schedule registration owner check (R2-14)

- [x] 3.1 Add the unknown-owner, suspended-owner and active-owner registration tests and run them RED
- [x] 3.2 Check `identity.users` inside the registration transaction, update `deploy/systemd/edge.conf.example`, and run them GREEN

## 4. Threads hosts (R2-11)

- [x] 4.1 Add `capture.rs::threads_dot_com_permalinks_are_accepted_for_the_threads_provider` and run it RED
- [x] 4.2 Add `threads.com` and `www.threads.com` to the Threads arm and run it GREEN

## 5. Chunk 413 in OpenAPI (R2-18)

- [ ] 5.1 Add `ai_archives.rs::the_chunk_route_documents_payload_too_large` and run it RED
- [ ] 5.2 Add the 413 `ResponseDoc`, regenerate `openapi/openapi.json` with `openapic`, and run it GREEN

## 6. Documentation corrections (R2-10, R2-01)

- [ ] 6.1 Correct the seed variable table in `deploy/nats/README.md` and state the Edge upstream owners in `deploy/README.md` and `deploy/systemd/edge.conf.example`. Documentation cannot start from a failing test.
