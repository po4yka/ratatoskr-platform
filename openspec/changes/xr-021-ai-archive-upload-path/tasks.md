## 1. Chunk limit

- [ ] 1.1 Add `crates/public-api/tests/ai_archives.rs::put_chunk_accepts_chunks_up_to_the_contract_maximum` (3 MiB and 16 MiB, transfer budget and public limit raised to 32 MiB) and run it RED on the 413 from axum's 2 MiB default
- [ ] 1.2 Wrap the chunk handler in `DefaultBodyLimit::max(CHUNK_SIZE_MAX_BYTES)` and run it GREEN

## 2. Media type at open

- [ ] 2.1 Add `opening_a_transfer_with_a_non_zip_media_type_is_refused` and run it RED (201 today)
- [ ] 2.2 Refuse `media_type != ARCHIVE_MEDIA_TYPE` in `open_transfer` and run it GREEN

## 3. Archive ceiling

- [ ] 3.1 Add `crates/core/tests/config_archive_and_digests.rs::archive_ceiling_defaults_to_two_gibibytes_and_rejects_out_of_range` (beside `config_validation.rs`, which is at the file-size limit), `ai_archives.rs::prepare_above_the_archive_ceiling_is_413_payload_too_large` and `prepare_accepts_an_archive_larger_than_the_transfer_body_budget` and run them RED
- [ ] 3.2 Add `archive_staging.max_archive_bytes`, rule V21, `ApiState.archive_max_bytes`, the 413 path and document, regenerate `openapi/openapi.json`, document the key in `.env.example`, `edge.conf.example` and `DEVELOPMENT.md`, and run 3.1 GREEN

## 4. Receipt forward

- [ ] 4.1 Change the receipt stub to POST (shared test support in `crates/public-api/tests/support/`) and add `ai_archives_receipt.rs::finalize_forwards_post_with_zip_content_type_and_all_claims` and `finalize_removes_the_staging_directory_after_success` and run them RED (405 and `upstream_invalid_response` today)
- [ ] 4.2 Move method and content type into `Gateway::forward_archive_receipt`, stream the verified file in 64 KiB frames (finalize moves to `archives/finalize.rs` to stay under the line cap), remove the staging directory after success, fix the `ArchivePrepared` doc line and run them GREEN

## 5. Receiver readiness

- [ ] 5.1 Add `capabilities.rs::archive_receiver_is_available_only_for_its_own_receipt_document` and run it RED (`service_available` only checks freshness)
- [ ] 5.2 Add `Gateway::archive_receiver_available` and use it in the Edge observer, which sets the readiness `archive_provider_ready` reads, amend ADR-0015, note it in `README.md` and `DEVELOPMENT.md`, and run it GREEN

## 6. Report validation

- [ ] 6.1 Add `projection.rs::a_partially_succeeded_report_without_a_diagnostic_is_rejected_and_leaves_the_operation_readable` and the positive one-warning test and run the first RED (applied today)
- [ ] 6.2 Call `report.validate()` in `ProgressReport::read` and run both GREEN
