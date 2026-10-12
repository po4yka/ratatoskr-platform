# ai-archive-acceptance Specification

## Purpose
TBD - created by archiving change xr-021-ai-archive-upload-path. Update Purpose after archive.

## Requirements

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

### Requirement: Abandoned archive staging is reaped

Edge SHALL run a bounded reaper every `archive_staging.sweep_interval_seconds` (default 300, rule V23, 30 to 3600) when an archive receiver is configured. A sweep SHALL handle at most 200 transfer sessions and 200 orphan directories, oldest first. It SHALL remove the token directory and then delete the row of a transfer that is expired, `failed`, `finalized` with a leftover directory, or owned by an operation terminal for more than 600 seconds. It SHALL remove a directory named `rst_*` that has no row and is older than 3600 seconds, and a `.readiness-*` file older than 3600 seconds. It SHALL NOT follow a symbolic link, SHALL NOT leave the staging root, and SHALL NOT touch an unexpired `open` transfer of a non-terminal operation.

#### Scenario: each reason removes exactly its directory and row

- **WHEN** a sweep runs over one expired, one failed, one finalized-with-directory and one terminal-operation transfer and one open transfer
- **THEN** the four reaped transfers lose their directory and row, each counted under its own reason, and the open transfer is untouched

#### Scenario: an orphan is removed only after its grace

- **WHEN** a sweep runs over an `rst_*` directory with no row that is 100 seconds old and another that is 4000 seconds old
- **THEN** only the older one is removed

#### Scenario: a symbolic link is not followed

- **WHEN** an entry named like a token is a symbolic link to a directory outside the root
- **THEN** the sweep removes nothing outside the root and does not remove the link target

#### Scenario: a failed removal leaves the row

- **WHEN** the directory of a reapable transfer cannot be removed
- **THEN** its row remains for the next sweep

### Requirement: Staging readiness is re-evaluated

Edge SHALL re-evaluate the archive staging readiness on every sweep: it is ready only while the root is writable and the volume has at least twice `archive_staging.max_archive_bytes` free.

#### Scenario: readiness follows the volume

- **WHEN** the root becomes unwritable, and later writable again
- **THEN** the staging readiness is false and then true again

### Requirement: Reopening replaces a dead session

`open_transfer` SHALL replay the stored token only for a `open` row that has not expired, and for a `finalized` row. A `failed` or expired row SHALL be replaced by a new session with a new token, its old directory removed, answering 201.

#### Scenario: a reopen after a digest mismatch mints a new token

- **WHEN** a session failed with a digest mismatch and the client opens the same operation again
- **THEN** the response is 201 with a new token, the old directory is gone and the old token answers 404 on status
