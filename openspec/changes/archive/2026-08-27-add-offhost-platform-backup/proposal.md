## Why

Platform's only durable recovery copies are currently on the Raspberry Pi: the daily PostgreSQL
dump on NVMe and Borg's copy on the second local volume. Losing the board therefore loses Platform
identity, operation, ingest state, and the configuration required to restore it.

## What Changes

- Add a root-run, bounded off-host replication job that encrypts the completed daily Platform dump,
  a configuration snapshot, and the Borg recovery material before uploading them to one
  S3-compatible bucket.
- Configure the endpoint, bucket, scoped S3 credentials, and public age recipient only through
  root-readable environment files; the age private key stays off the board and no secret is tracked.
- Add independent remote-retention handling and a documented bucket lifecycle policy.
- Add a weekly systemd restore drill that downloads yesterday's off-host object, decrypts it using
  an off-board key supplied only to the drill host, restores it into a scratch PostgreSQL database,
  and emits unambiguous pass/fail output.
- Document installation, dry-run evidence, a recovery-to-new-board procedure that starts only from
  the off-host copy, and why continuous WAL shipping remains deferred.

## Capabilities

### New Capabilities

- `off-host-platform-backup`: encrypted, independently retained, and regularly restored off-host
  copies of Platform's PostgreSQL state and deployment configuration.

### Modified Capabilities

- None.

## Impact

- Adds deployment backup scripts, systemd service/timer units, checked example environment files,
  shell tests and CI lint coverage.
- Requires documented host packages for age and one maintained S3-compatible client, plus a
  dedicated least-privilege bucket and an age identity retained off the Raspberry Pi.
- Does not change Platform APIs, schemas, service-owned blobs, or continuous-WAL/multi-cloud policy.
