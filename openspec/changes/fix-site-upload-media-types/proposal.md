## Why

The SITE-027 task 12.3 integration run reaches consent and sign-in but a draft blob upload returns
502. Edge sends the Notes upload open and finalize bodies without `Content-Type`. The real Notes
JSON extractor refuses them; the old test stub accepts any media type.

The workspace change `add-site-owner-editing-and-live`, spec `site-owner-editing`, owns the public
contract. This change corrects only the local Notes adapter.

## What Changes

- Send `application/json` for the upload open and finalize control requests to Notes.
- Send `application/octet-stream` for the raw chunk request.
- Add a regression test with real JSON extraction in the Notes stub and a strict chunk media type.

## Capabilities

### New Capabilities

- `site-upload-media-types`: the media types of the Notes requests made for a site upload.

### Modified Capabilities

None.

## Impact

`crates/public-api/src/site_edits.rs` and its tests. No dependency, schema, public contract or host
configuration changes. Existing clients require no change.
