## 1. Notes upload request media types

- [x] 1.1 RED: add `site_upload_media_types.rs::site_upload_succeeds_when_notes_requires_its_request_media_types` with Axum JSON extraction on Notes open/finalize and a strict octet-stream chunk. Confirm that the public status assertion fails with 502 instead of 201 because Notes refuses the missing media type.
- [x] 1.2 GREEN: send JSON headers on both control requests and an octet-stream header on the raw chunk. Retain the original claims for cleanup and GET calls. Verify 1.1, the site-edit and upload-failure suites, and the DEVELOPMENT.md gate. No separate test for these planning documents.
