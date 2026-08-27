## Fixture-backed dry-run evidence

Recorded 2026-08-27T06:50:02Z on the local CI-equivalent checkout. No credential, bucket endpoint,
or recovery identity was used.

| Command | Fixture target | Observed result |
|---|---|---|
| `bash deploy/backup/tests/offhost_backup_test.sh` | generated age identity, fake `aws` writing only below an ephemeral `MOCK_S3_ROOT`, fake Borg export | exit 0; `PASS: off-host replication tests`. The `replication_dry_run_does_not_upload` case observed the replication script's `DRY-RUN` output and asserted that neither an S3 object nor NVMe-stage file was created. |
| `bash deploy/backup/tests/offhost_drill_test.sh` | generated age identity, fake S3 read path, disposable local PostgreSQL 17 container | exit 0; `PASS: off-host drill tests`. The `drill_dry_run_does_not_write_a_database` case observed the drill's `DRY-RUN` output and asserted that no scratch database was created. |
| `docker run --rm -v "$PWD/deploy/backup:/backup:ro" debian:12 ... systemd-analyze verify ...` | read-only checkout mount and temporary Debian 12 container | exit 0; all four Pi/verifier service and timer units were accepted after their scripts were made visible at the documented install paths. |

The first two fixture clients have no network implementation: their S3 root is a local temporary
directory. Consequently these results are dry-run and local restore evidence, not a claim that a
provider bucket, credentials, or a deployment target was contacted.
