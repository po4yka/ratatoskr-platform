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

The shipped route table SHALL route `/v1/notes` to the loopback listener `127.0.0.1:8099` with the class `control`. Requests under `/v1/notes/connections/{id}/blobs`, and requests to `/v1/notes/connections/{id}/changes` and `/v1/notes/connections/{id}/manifest`, SHALL use the `transfer` budget.

#### Scenario: A blob upload is larger than the control budget

- **WHEN** a paired plugin sends an upload chunk above the control body budget to `/v1/notes/connections/{id}/blobs/{token}/chunks/{index}`
- **THEN** the stub upstream receives the chunk unchanged, verified by `crates/public-api/tests/notes_routes.rs::blob_upload_uses_the_transfer_budget`

#### Scenario: Notes answers a change batch after the control timeout

- **WHEN** the stub upstream answers a change batch and a manifest after two seconds, the `control` response budget is one second and the `transfer` response budget is ten seconds
- **THEN** the paired plugin receives both answers, verified by `crates/public-api/tests/notes_device_routes.rs::change_batch_and_manifest_take_the_transfer_timeout`

#### Scenario: A streamed blob upload is larger than the transfer budget

- **WHEN** a paired plugin sends an upload chunk without `Content-Length` that streams more bytes than the transfer budget to `/v1/notes/connections/{id}/blobs/{token}/chunks/{index}`
- **THEN** Edge answers `413` with `platform.request.payload_too_large`, and not `503`, verified by `crates/public-api/tests/notes_connect.rs::oversized_streamed_blob_upload_is_refused_with_413`

### Requirement: Edge bounds a notes device body by the limit of notes

Edge SHALL forward a change batch up to 2 MiB, a manifest up to 32 MiB and an upload chunk at `/v1/notes/connections/{id}/blobs/{token}/chunks/{index}` up to 16 MiB, the chunk maximum of the shared transfer contract, and SHALL refuse a larger body with `413` before it reaches notes. A limit is never above the `transfer` budget.

#### Scenario: A body of the largest size that notes takes

- **WHEN** a paired plugin sends a change batch of 2 MiB, a manifest of 32 MiB and an upload chunk of 16 MiB, and then each of them with one byte more
- **THEN** the stub upstream receives each body of the largest size unchanged, and Edge refuses each larger body with `413` (`platform.request.payload_too_large`) before it reaches the stub, verified by `crates/public-api/tests/notes_device_routes.rs::change_batch_admits_the_notes_maximum`, `manifest_admits_the_notes_maximum` and `chunk_admits_the_contract_maximum`

#### Scenario: A streamed body grows past the largest size that notes takes

- **WHEN** a paired plugin sends a change batch and a manifest without `Content-Length` that stream one MiB more than 2 MiB and 32 MiB, under a `transfer` budget of 100 MiB
- **THEN** Edge answers each with `413` (`platform.request.payload_too_large`), and the stub upstream records no request, verified by `crates/public-api/tests/notes_device_routes.rs::streamed_change_batch_and_manifest_past_the_notes_maximum_are_refused_with_413`

### Requirement: A notes device route needs a device credential

Edge SHALL refuse with `403` (`platform.auth.forbidden`), and SHALL NOT forward, a request to `/v1/notes/connections/{id}/changes`, `/v1/notes/connections/{id}/manifest` or a path under `/v1/notes/connections/{id}/blobs` whose credential is not the credential of an active paired device. Notes answers such a request with `401`, which Edge does not relay. The other notes routes SHALL go to notes with the user id claim of an authenticated session of the owner that is not a device.

#### Scenario: The owner's browser session calls the device routes and the read routes

- **WHEN** the owner's browser session sends a change batch, a manifest and each upload request, and then reads the connections, the files, a file at a revision and its index, sets the excluded folders and removes the connection
- **THEN** Edge answers each device request with `403` and the stub upstream does not receive it, and the stub receives each other request with its method, path and query unchanged, the owner's user id and no device claim, verified by `crates/public-api/tests/notes_device_routes.rs::owner_session_is_refused_on_device_routes`

### Requirement: A device credential reaches only the connect and the device routes of notes

Edge SHALL refuse with `403` (`platform.auth.forbidden`), and SHALL NOT forward, a request of an active paired device to a notes path other than `POST /v1/notes/connections` and the device routes: `GET /v1/notes/connections`, `…/{id}/files`, `…/{id}/files/{path}`, `…/{id}/index/{path}`, `PUT …/{id}/excluded-folders` and `DELETE …/{id}` included. The Obsidian plugin and the export agent call only the connect and the device routes.

#### Scenario: A paired plugin calls the owner routes

- **WHEN** a paired plugin reads the connections, the files, a file at a revision and its index, sets the excluded folders, removes the connection, and then sends a connect
- **THEN** Edge answers each of the first six with `403` and the stub upstream does not receive them, and the stub receives the connect, verified by `crates/public-api/tests/notes_device_routes.rs::device_credential_is_refused_on_owner_routes`

### Requirement: Edge does not relay a redirect of a domain service

The generic proxy SHALL answer a redirect of a domain service (`301`, `302`, `303`, `307`, `308`, and every other `3xx` except `304`) with `502` (`edge.upstream_invalid_response`), and SHALL NOT relay its `Location` header. A `304` has no body and no `Location`, and it SHALL be relayed.

#### Scenario: Notes answers a file read with a redirect

- **WHEN** the stub notes upstream answers a file read of the owner with `301`, `302`, `303`, `307` and `308`, each with a `Location` header
- **THEN** Edge answers each with `502` and `edge.upstream_invalid_response` and no `Location` header, verified by `crates/public-api/tests/notes_device_routes.rs::redirect_from_a_notes_route_is_not_relayed`

### Requirement: Edge reports the notes capability where it routes notes

`GET /v1/capabilities` SHALL list `notes.vaults` to an authenticated caller when the deployment routes the notes service and the database is reachable, and SHALL NOT list it otherwise. The notes service serves no capability document, so the name does not depend on a sample of notes.

#### Scenario: The capability follows the notes route

- **WHEN** the owner reads the capabilities of a deployment with the notes route, of one without it, and of one with the route and an unreachable database
- **THEN** only the first answer lists `notes.vaults`, verified by `crates/public-api/tests/capabilities.rs::notes_vaults_follows_the_notes_route`

### Requirement: Edge forwards a notes read unchanged and streams its bytes

The generic proxy SHALL forward the path and the query of a notes read to notes as the client sent them: no percent-decoding, no change of `%2F` into `/`, and no removal of a dot segment. It SHALL stream the bytes of a file to the client as notes sends them, and SHALL NOT wait for the end of the body.

#### Scenario: A file path with encoded and nested segments

- **WHEN** the owner reads `files/A%20B/Note%201.md`, `files/A%20B/C%2FD/%2E%2E/Caf%C3%A9%20%25%2B.md?revision=<sha256>`, a path of five segments, and `index/A%20B/C%2FD/Note%201.md`
- **THEN** the stub upstream receives each path and query byte for byte, verified by `crates/public-api/tests/notes_device_routes.rs::file_path_segments_reach_notes_unchanged`

#### Scenario: Notes holds the rest of a file

- **WHEN** the stub upstream sends the first bytes of a file and holds the rest
- **THEN** the client receives the first bytes before the stub sends the rest, verified by `crates/public-api/tests/notes_device_routes.rs::file_bytes_stream_without_buffering`

### Requirement: The connect proxy keeps no part of the body

`POST /v1/notes/connections` SHALL mint an operation of kind `notes.connection.connect`, forward the body to the notes upstream unchanged with `x-ratatoskr-operation-id`, and answer `202` with the operation id. Edge SHALL NOT write any part of the body to its database, its outbox, its idempotency records, its audit records or its logs.

#### Scenario: Edge handles a connect request with marker secrets

- **WHEN** a test sends a connect request whose password fields hold known markers through Edge to a stub notes upstream, and then searches every Platform table and the captured log output for the markers
- **THEN** no marker is found, and the stub received the body unchanged with the minted operation id, verified by `crates/public-api/tests/notes_routes.rs::connect_body_is_not_persisted_or_logged`

### Requirement: A connect that notes does not take ends its operation

When the forward of `POST /v1/notes/connections` fails after Edge minted the operation, Edge SHALL terminate the operation as `failed` through the one transition applier before it answers, with an error whose code is the code of the Edge fault, or the code of the error envelope of the notes service when notes answered one. The error and the progress entry SHALL hold the fixed message `The connect did not complete.` and no part of the notes answer other than its code.

#### Scenario: The notes service is not reachable

- **WHEN** a connect is sent through Edge and nothing listens on the notes listener
- **THEN** Edge answers `503` and the operation is `failed` with `edge.upstream_unavailable`, verified by `crates/public-api/tests/notes_routes.rs::unreachable_notes_fails_the_connect_operation`

#### Scenario: The notes service does not answer in time

- **WHEN** a connect is sent through Edge and the stub notes upstream answers after the control budget
- **THEN** Edge answers `504` and the operation is `failed` with `edge.upstream_timeout`, verified by `crates/public-api/tests/notes_routes.rs::timed_out_notes_fails_the_connect_operation`

#### Scenario: The notes service refuses the connect

- **WHEN** the stub notes upstream answers a connect with `409` and the error envelope `notes.connection.vault_already_connected`
- **THEN** Edge relays the `409` and the operation is `failed` with `notes.connection.vault_already_connected`, verified by `crates/public-api/tests/notes_routes.rs::refused_connect_fails_the_operation_with_the_notes_code`

#### Scenario: The operation keeps no notes text

- **WHEN** the stub notes upstream refuses a connect with an error envelope whose message holds a marker
- **THEN** Edge relays the envelope to the client, the error and the progress entry of the operation hold `The connect did not complete.`, and no row of `operations.operation_errors` or `operations.operation_progress` holds the marker, verified by `crates/public-api/tests/notes_connect.rs::refused_connect_keeps_no_notes_text`

### Requirement: A client disconnect does not stop the end of a connect

Edge SHALL do the forward of a connect and the terminal write of its operation in a task that a client disconnect does not stop. The task SHALL end within the `control` response budget of the notes route.

#### Scenario: The client disconnects before notes answers

- **WHEN** a connect reaches the stub notes upstream, the client disconnects, and the stub then refuses the connect with `409`
- **THEN** the operation is `failed` with `notes.connection.vault_already_connected`, verified by `crates/public-api/tests/notes_connect.rs::client_disconnect_does_not_stop_the_terminal_write`

#### Scenario: Notes stops in the middle of its answer

- **WHEN** the stub notes upstream answers a connect with `202` and never ends the body of the answer, and the `control` response budget is one second
- **THEN** Edge answers `504` within the budget and the operation is `failed` with `edge.upstream_timeout`, verified by `crates/public-api/tests/notes_connect.rs::stalled_connect_answer_fails_within_the_budget`

### Requirement: A connect body that fails is a request error

A connect body that fails while Edge sends it to notes SHALL be refused with `400` (`platform.request.invalid`), and its operation SHALL be `failed` with that code, not with `edge.upstream_unavailable`.

#### Scenario: The request body fails while Edge sends it

- **WHEN** a client sends a part of a connect body and then the body fails
- **THEN** Edge answers `400` and the operation is `failed` with `platform.request.invalid`, verified by `crates/public-api/tests/notes_connect.rs::aborted_connect_body_is_a_request_error`

### Requirement: Edge accepts only 202 from notes for a connect

An answer of notes to a connect that is not `202` and not an error envelope SHALL be `502` (`edge.upstream_invalid_response`), Edge SHALL NOT relay it, and the operation SHALL be `failed` with that code.

#### Scenario: Notes answers a redirect

- **WHEN** the stub notes upstream answers a connect with `307` and a `Location` header
- **THEN** Edge answers `502` with `edge.upstream_invalid_response` and no `Location` header, and the operation is `failed` with that code, verified by `crates/public-api/tests/notes_connect.rs::redirect_from_notes_is_an_invalid_response`

#### Scenario: Notes answers a success other than 202

- **WHEN** the stub notes upstream answers a connect with `200` and a JSON body
- **THEN** Edge answers `502` and the operation is `failed` with `edge.upstream_invalid_response`, verified by `crates/public-api/tests/notes_connect.rs::success_other_than_202_is_an_invalid_response`

### Requirement: The connect keeps its body budget while the body streams

A connect body that declares no length and grows above the `control` body budget of the notes route SHALL be refused with `413` (`platform.request.payload_too_large`), and its operation SHALL be `failed` with that code.

#### Scenario: A streamed connect body is too large

- **WHEN** a connect body without `Content-Length` streams more bytes than the `control` budget through Edge
- **THEN** Edge answers `413` and the operation is `failed` with `platform.request.payload_too_large`, verified by `crates/public-api/tests/notes_routes.rs::oversized_streamed_connect_is_refused_with_413`

### Requirement: Edge refuses a connect at the path with a trailing slash

Edge SHALL refuse `POST /v1/notes/connections/` with `404` and SHALL NOT forward it, so a connect cannot reach the notes service through the generic proxy, which keeps no operation and does not protect the body. Other methods at that path SHALL go through the generic proxy.

#### Scenario: A connect with a trailing slash

- **WHEN** an authenticated client sends `POST /v1/notes/connections/` and then `GET /v1/notes/connections/`
- **THEN** Edge answers the `POST` with `404` and the stub notes upstream does not receive it, and the stub receives the `GET`, verified by `crates/public-api/tests/notes_routes.rs::connect_with_a_trailing_slash_is_refused_at_edge`

### Requirement: The public limits hold every budget that a route can select

The configuration SHALL be refused when `public.max_body_bytes` or `public.request_timeout_seconds` is below the budget of the class of a configured route, or below the `transfer` budget when that route has a sub-path that uses the `transfer` budget.

#### Scenario: The notes route with public limits that hold only the control budget

- **WHEN** Edge loads a configuration with the notes route (`control`) and public limits equal to the `control` budget
- **THEN** the configuration is refused for `public.max_body_bytes` and `public.request_timeout_seconds`, and limits equal to the `transfer` budget are accepted, verified by `crates/core/tests/config_validation.rs::notes_blob_path_needs_the_public_transfer_limits`
