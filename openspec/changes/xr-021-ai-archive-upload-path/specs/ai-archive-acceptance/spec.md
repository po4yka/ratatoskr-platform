## ADDED Requirements

### Requirement: Chunk uploads accept the contract maximum

Platform SHALL accept an archive chunk of any size up to the contract maximum of 16777216 bytes on the chunk route, and SHALL keep rejecting a chunk whose length differs from the declared chunk length.

#### Scenario: a 16 MiB chunk is accepted

- **WHEN** a client uploads a chunk of 16 MiB to a transfer whose declared chunk size is 16 MiB and the deployment budget allows it
- **THEN** the chunk is stored and the response is a success

### Requirement: Transfers must declare a zip media type

Platform SHALL refuse to open a transfer whose declared media type is not `application/zip`.

#### Scenario: a non-zip media type is refused at open

- **WHEN** a client opens a transfer declaring `application/octet-stream`
- **THEN** the response is 400 `invalid_request` and no transfer exists

### Requirement: An explicit archive size ceiling

Platform SHALL bound the declared size of an archive by `archive_staging.max_archive_bytes` (default 2147483648, range 1048576 to 10737418240), independent of the per-request body budget, and SHALL answer an oversize declaration with 413 `payload_too_large`.

#### Scenario: an archive above the ceiling is refused with 413

- **WHEN** a client prepares an archive whose declared size exceeds the ceiling
- **THEN** the response is 413 with the `payload_too_large` error

#### Scenario: an archive above the request budget but below the ceiling is accepted

- **WHEN** the transfer body budget is 4 MiB, the ceiling is 8 MiB and a client prepares a 6 MiB archive
- **THEN** the response is 202

### Requirement: Finalize streams a POST receipt and cleans staging

Platform SHALL forward a verified archive to the receiver with `POST`, `Content-Type: application/zip`, a `Content-Length` equal to the declared size and all six claim headers, streaming the body from the verified file, and SHALL remove the transfer's staging directory after a successful forward.

#### Scenario: the receiver sees the contract request

- **WHEN** a multi-chunk archive is finalized against a stub receiver
- **THEN** the stub receives a POST with the zip content type, the declared length, the six claim headers and a body byte-identical to the archive

#### Scenario: staging is removed after success

- **WHEN** finalize succeeds
- **THEN** the transfer's staging directory no longer exists

### Requirement: Receiver readiness requires the receipt capability document

Platform SHALL treat an archive receiver as available only when its last probe is fresh and its capability document is the receipt capability document for that provider.

#### Scenario: a fresh but wrong document is not ready

- **WHEN** a receiver answers the probe with an empty document, or with the other provider's document, or with 404
- **THEN** the receiver is not available

### Requirement: Invalid operation reports are rejected, not applied

Platform SHALL record an operation report that fails contract validation as rejected and SHALL leave the stored operation snapshot unchanged and readable.

#### Scenario: a partially succeeded report without a diagnostic

- **WHEN** a report with status `partially_succeeded` and neither a warning nor an error is delivered
- **THEN** the outcome is rejected and reading the operation still answers with its previous status
