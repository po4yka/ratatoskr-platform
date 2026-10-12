## ADDED Requirements

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
