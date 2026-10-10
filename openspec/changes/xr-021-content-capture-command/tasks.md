## 1. Typed content capture command

- [ ] 1.1 Add `crates/public-api/tests/capture_blobs.rs::capture_outbox_row_decodes_as_content_capture_requested` and the webhook equivalent in `crates/ingest/tests/webhook.rs` and run them RED (the row holds the legacy document)
- [ ] 1.2 Build the `CommandEnvelope` with `ContentCaptureRequested` once, in `platform_eventing::content_capture`, for `captures.rs` and the ingest adapter; update the two webhook tests that asserted the removed shape (the eventing fixtures only use the subject, so they are unchanged; `command.rs` stays) and run them GREEN

## 2. Blob capture route

- [ ] 2.1 Add the seven blob capture tests in `crates/public-api/tests/capture_blobs.rs` and `tools/openapic/tests/document.rs::the_capture_and_channel_digest_operations_are_documented` (the operation-id inventory, which did not exist) and run them RED (404 today)
- [ ] 2.2 Add `SubmitBlobCapture`, the `submit_blob` handler sharing a generalised `accept` (now `intake.rs`), validation and authorization per S11, the RouteDoc, the table and schema registration, regenerate `openapi/openapi.json` and run them GREEN

## 3. Social report characterization

- [ ] 3.1 Add `crates/public-api/tests/operations.rs::social_capture_reports_are_visible_with_the_codes_the_extension_reads`; it is expected green today because Platform needs no projection change, and a red result would make the projection the defect to fix
