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

### Requirement: A connect that notes does not take ends its operation

When the forward of `POST /v1/notes/connections` fails after Edge minted the operation, Edge SHALL terminate the operation as `failed` through the one transition applier before it answers, with an error whose code is the code of the Edge fault, or the code of the error envelope of the notes service when notes answered one. The error SHALL hold no part of the notes answer other than its code.

#### Scenario: The notes service is not reachable

- **WHEN** a connect is sent through Edge and nothing listens on the notes listener
- **THEN** Edge answers `503` and the operation is `failed` with `edge.upstream_unavailable`, verified by `crates/public-api/tests/notes_routes.rs::unreachable_notes_fails_the_connect_operation`

#### Scenario: The notes service does not answer in time

- **WHEN** a connect is sent through Edge and the stub notes upstream answers after the control budget
- **THEN** Edge answers `504` and the operation is `failed` with `edge.upstream_timeout`, verified by `crates/public-api/tests/notes_routes.rs::timed_out_notes_fails_the_connect_operation`

#### Scenario: The notes service refuses the connect

- **WHEN** the stub notes upstream answers a connect with `409` and the error envelope `notes.connection.vault_already_connected`
- **THEN** Edge relays the `409` and the operation is `failed` with `notes.connection.vault_already_connected`, verified by `crates/public-api/tests/notes_routes.rs::refused_connect_fails_the_operation_with_the_notes_code`

### Requirement: The connect keeps its body budget while the body streams

A connect body that declares no length and grows above the `control` body budget of the notes route SHALL be refused with `413` (`platform.request.payload_too_large`), and its operation SHALL be `failed` with that code.

#### Scenario: A streamed connect body is too large

- **WHEN** a connect body without `Content-Length` streams more bytes than the `control` budget through Edge
- **THEN** Edge answers `413` and the operation is `failed` with `platform.request.payload_too_large`, verified by `crates/public-api/tests/notes_routes.rs::oversized_streamed_connect_is_refused_with_413`

### Requirement: Edge refuses a connect at any other path

Edge SHALL refuse `POST /v1/notes/connections/` with `404` and SHALL NOT forward it, so a connect cannot reach the notes service through the generic proxy, which keeps no operation and does not protect the body. Other methods at that path SHALL go through the generic proxy.

#### Scenario: A connect with a trailing slash

- **WHEN** an authenticated client sends `POST /v1/notes/connections/` and then `GET /v1/notes/connections/`
- **THEN** Edge answers the `POST` with `404` and the stub notes upstream does not receive it, and the stub receives the `GET`, verified by `crates/public-api/tests/notes_routes.rs::connect_with_a_trailing_slash_is_refused_at_edge`

### Requirement: The public limits hold every budget that a route can select

The configuration SHALL be refused when `public.max_body_bytes` or `public.request_timeout_seconds` is below the budget of the class of a configured route, or below the `transfer` budget when that route has a sub-path that uses the `transfer` budget.

#### Scenario: The notes route with public limits that hold only the control budget

- **WHEN** Edge loads a configuration with the notes route (`control`) and public limits equal to the `control` budget
- **THEN** the configuration is refused for `public.max_body_bytes` and `public.request_timeout_seconds`, and limits equal to the `transfer` budget are accepted, verified by `crates/core/tests/config_validation.rs::notes_blob_path_needs_the_public_transfer_limits`
