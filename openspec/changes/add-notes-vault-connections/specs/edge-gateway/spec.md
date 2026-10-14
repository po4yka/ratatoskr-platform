## MODIFIED Requirements

### Requirement: Identity crosses only as minted claims

The system SHALL strip every inbound `x-ratatoskr-*` header and forward only the authenticated user id, the optional device id, the optional device kind (`x-ratatoskr-device-kind`, the stored kind of the authenticated device), and the minted correlation id.

#### Scenario: Client-supplied identity headers are replaced

- **WHEN** an authenticated request includes forged `x-ratatoskr-*` headers
- **THEN** the downstream receives only Edge-minted claims for that authenticated principal

#### Scenario: A device request names its device kind

- **WHEN** a paired `obsidian_plugin` device sends a request through Edge to a stub notes upstream, with forged user, device and device-kind headers
- **THEN** the stub receives the owner, the device id and the kind `obsidian_plugin` once each, and none of the forged values, verified by `crates/public-api/tests/notes_routes.rs::device_claim_names_the_device_kind`

## ADDED Requirements

### Requirement: Edge routes the notes service

The shipped route table SHALL route `/v1/notes` to the loopback listener `127.0.0.1:8099` with the class `control`. Requests under `/v1/notes/connections/{id}/blobs` SHALL use the `transfer` budget.

#### Scenario: A blob upload is larger than the control budget

- **WHEN** an authenticated client sends a body above the control body budget to `/v1/notes/connections/{id}/blobs` and to `/v1/notes/connections/{id}/changes`
- **THEN** the stub upstream receives the blob body unchanged, and Edge refuses the change batch with `413` before it reaches the stub, verified by `crates/public-api/tests/notes_routes.rs::blob_upload_uses_the_transfer_budget`

### Requirement: The connect proxy keeps no part of the body

`POST /v1/notes/connections` SHALL mint an operation of kind `notes.connection.connect`, forward the body to the notes upstream unchanged with `x-ratatoskr-operation-id`, and answer `202` with the operation id. Edge SHALL NOT write any part of the body to its database, its outbox, its idempotency records, its audit records or its logs.

#### Scenario: Edge handles a connect request with marker secrets

- **WHEN** a test sends a connect request whose password fields hold known markers through Edge to a stub notes upstream, and then searches every Platform table and the captured log output for the markers
- **THEN** no marker is found, and the stub received the body unchanged with the minted operation id, verified by `crates/public-api/tests/notes_routes.rs::connect_body_is_not_persisted_or_logged`
