## Purpose

Keep the local site-upload adapter compatible with the media types that the Notes transfer routes require.

## ADDED Requirements

### Requirement: Site uploads declare the Notes request media types

Edge SHALL send `application/json` on the Notes upload open and finalize requests, and
`application/octet-stream` on the raw chunk request. It SHALL retain the content bytes and the
existing site owner claims.

#### Scenario: Notes enforces each upload request media type

- **WHEN** a site owner uploads a draft and the Notes stub uses the real JSON extractor for both control requests and requires an octet-stream chunk
- **THEN** the upload returns 201 with its content digest and all three steps complete, verified by `site_upload_media_types.rs::site_upload_succeeds_when_notes_requires_its_request_media_types`
