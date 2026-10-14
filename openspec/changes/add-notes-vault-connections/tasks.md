## 1. Notes route, device kind and connect proxy (workspace 9.1, 9.2)

- [x] 1.1 RED: `crates/public-api/tests/notes_routes.rs::device_claim_names_the_device_kind` asserts that a stub notes upstream receives a minted claim with the owner, the device id and the kind `obsidian_plugin`, and no client-supplied identity header; `connect_body_is_not_persisted_or_logged` sends a connect body with marker secrets and asserts that no Platform table, the outbox, the idempotency and audit records, and the captured log output hold a marker, and that the stub received the body unchanged with the minted operation id; `blob_upload_uses_the_transfer_budget` asserts that a body above the control budget reaches the stub through `/blobs` and is refused on `/changes`. Confirm that all three fail because the route does not exist.
- [x] 1.2 GREEN: add the `notes` entry to the route table (`/v1/notes`, loopback 8099, `control`, and `transfer` for `/blobs`), the device kind `obsidian_plugin`, the claim that names the device kind, and the connect proxy that mints the operation id and excludes the body from idempotency storage, audit and logs. Verify the three tests of 1.1.

## 2. Report authority of the connect operation (workspace 9.3, 9.4)

- [x] 2.1 RED: `crates/operations/tests/report_authority.rs::notes_connect_accepts_only_the_notes_lane` asserts that a report from the X identity for a `notes.connection.connect` operation is rejected and the status is unchanged, and that the report of `ratatoskr-notes` is applied. Confirm that it fails because the kind is not minted.
- [x] 2.2 GREEN: add the authority row and the mint site. Verify 2.1 and `every_minted_operation_kind_has_an_authority_row`.

## 3. NOTES identity and Knowledge durables (workspace 9.5, 9.6)

- [x] 3.1 RED: extend `crates/eventing/tests/nats_permissions.rs` with the case `notes_identity_publishes_its_events_and_knowledge_reads_its_durables`, which asserts that `NOTES` can publish `evt.notes.file.changed.v1` and that Knowledge can fetch from `ratatoskr_knowledge_notes_files` and `ratatoskr_knowledge_notes_vaults`. Confirm that it fails because the identity and the durables do not exist.
- [ ] 3.2 GREEN: add the `NOTES` stanza to `deploy/nats/ratatoskr.conf` and the two durables to `crates/eventing/src/stream.rs`, and move the topology tables of the existing tests to fourteen identities and 25 service durables. Verify 3.1.
