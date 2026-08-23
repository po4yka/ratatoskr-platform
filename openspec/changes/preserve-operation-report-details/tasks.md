## 1. Reported result persistence

- [x] 1.1 Add `a_success_report_round_trips_complete_blob_ref` to `crates/operations/tests/projection.rs` and run it RED; its assertion that the snapshot result blob equals the report blob must fail because the current value is `None`
- [x] 1.2 Store and reload the structured `BlobRef` through the current `schema.sql` and projection transaction, then run `a_success_report_round_trips_complete_blob_ref` GREEN

## 2. Reported diagnostic persistence

- [x] 2.1 Add `a_failed_report_round_trips_error_and_warnings` to `crates/operations/tests/projection.rs` and run it RED; reading the failed snapshot must fail for a missing error before its error and warning equality assertions can pass
- [x] 2.2 Store and reload complete typed error and warning envelopes in the projection transaction, then run `a_failed_report_round_trips_error_and_warnings` GREEN

## 3. Repository verification

- [x] 3.1 Run the operations crate tests, fresh-schema tests, `cargo fmt --all -- --check`, and the repository Clippy gate; this broad verification follows the green behavior tests and adds no behavior
- [x] 3.2 Run the complete gate documented in `DEVELOPMENT.md`, inspect the final diff including generated artifacts, and verify only the intended change plus the pre-existing unstaged `AGENTS.md` edit remains; this verification task adds no behavior
