# Backup, off-host recovery, and the restore that proves it

A backup nobody has restored is a file. Platform keeps a short local recovery path for a failed
disk and an encrypted off-host recovery path for loss of the Raspberry Pi.

## Scope and recovery copies

Platform state is the `ratatoskr` PostgreSQL custom dump: `identity`, `operations`, and
`platform_ingest`. The existing daily dump writes to `/mnt/nvme/backups/ratatoskr` at 02:30; the
host's Borg job copies `/mnt/nvme` to `/mnt/backup` at 03:00. That second volume is useful for a disk
failure but is **not** off-host.

At 04:00, `ratatoskr-offhost-backup.timer` makes a dated recovery set in one S3-compatible bucket:

- the completed PostgreSQL custom dump;
- an export of the latest completed Borg archive; and
- an allowlisted configuration archive: the three Platform environment files and units, NATS
  configuration, and Platform's logrotate file.

Each object is encrypted with the configured public age recipient before upload. The Pi has no age
private identity. The configuration archive intentionally excludes
`/etc/ratatoskr/offhost-backup.env`, every age identity, and the NATS nkey seed: the S3 access
credential and recovery identity are operator-held bootstrap credentials, while the nkey is
regenerated following [`deploy/nats/README.md`](../nats/README.md).

`JetStream` is not backed up. It is a replayable delivery cache around `operations.outbox` and the
producers' event records; restoring it could replay work the database says is complete. Service-owned
Vault and AI blobs are also out of scope: their owners replicate them independently.

## Install the Pi replication job

The Pi needs Debian's `age`, AWS CLI v2 and Borg client. AWS CLI receives S3-compatible endpoint,
bucket and scoped upload credentials from the root-only environment file; no remote address or secret
is committed here.

```bash
sudo apt-get install age awscli borgbackup
sudo install -d -m 0700 /mnt/nvme/backups/ratatoskr/offhost-stage
sudo install -m 0755 deploy/backup/ratatoskr-dump.sh /usr/local/bin/ratatoskr-dump.sh
sudo install -m 0755 deploy/backup/ratatoskr-offhost-backup.sh /usr/local/bin/ratatoskr-offhost-backup.sh
sudo install -m 0600 -o root -g root deploy/backup/offhost-backup.env.example \
  /etc/ratatoskr/offhost-backup.env
sudoedit /etc/ratatoskr/offhost-backup.env
sudo cp deploy/backup/ratatoskr-backup.{service,timer} \
  deploy/backup/ratatoskr-offhost-backup.{service,timer} /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ratatoskr-backup.timer ratatoskr-offhost-backup.timer
sudo systemctl start ratatoskr-backup.service
```

Set `RATATOSKR_BORG_REPOSITORY` to the repository the existing root Borg job writes. If the host's
03:00 Borg schedule changes or cannot finish within its window, move the 04:00 timer with it. The
replication job fails rather than silently uploading an older archive.

The Pi credential is limited to `ListBucket`, `GetObject`, and `PutObject` under the Platform prefix.
It has neither `DeleteObject` nor permission to change lifecycle rules.

### Remote retention is administered at the bucket

Local retention is fourteen dumps; remote retention is independently ninety days. Generate the
checked recommendation with the script (the committed
[`offhost-lifecycle-90-days.json`](offhost-lifecycle-90-days.json) is its 90-day output):

```bash
deploy/backup/ratatoskr-offhost-lifecycle.sh --remote-keep-days 90 \
  > /tmp/ratatoskr-offhost-lifecycle.json
```

An S3 administrator — not the Pi upload credential — applies and reads it back:

```bash
aws s3api put-bucket-lifecycle-configuration --bucket "$RECOVERY_BUCKET" \
  --lifecycle-configuration file:///tmp/ratatoskr-offhost-lifecycle.json
aws s3api get-bucket-lifecycle-configuration --bucket "$RECOVERY_BUCKET"
```

It expires each `postgresql/`, `borg/`, and `configuration/` prefix after 90 days and aborts
incomplete multipart uploads after seven days. Object-lock, bucket versioning, or a second cloud are
not required by this change; enable them only as a separately designed storage policy.

### Mandatory dry-runs

These commands validate inputs and show intended work without uploading, exporting Borg data, or
creating a database:

```bash
sudo sh -c 'set -a; . /etc/ratatoskr/offhost-backup.env; set +a; \
  /usr/local/bin/ratatoskr-offhost-backup.sh --dry-run'
sudo /usr/local/bin/ratatoskr-offhost-lifecycle.sh --remote-keep-days 90 --dry-run > /dev/null
```

## Install the separate verifier

The verifier is a recovery consumer, not a second Ratatoskr host: it runs no Platform binary,
listener, durable Platform database, or scheduler. It is a separate Linux system with systemd,
Docker, PostgreSQL 17 in a disposable container, `age`, and AWS CLI v2. It holds the private age
identity outside the Pi's failure domain.

```bash
sudo apt-get install age awscli docker.io
sudo install -d -m 0700 /var/lib/ratatoskr-offhost-drill /etc/ratatoskr
sudo install -m 0755 deploy/backup/ratatoskr-offhost-drill.sh /usr/local/bin/ratatoskr-offhost-drill.sh
sudo install -m 0600 -o root -g root deploy/backup/offhost-drill.env.example \
  /etc/ratatoskr/offhost-drill.env
sudo install -m 0600 -o root -g root /secure/offboard/offhost-recovery.agekey \
  /etc/ratatoskr/offhost-recovery.agekey
sudoedit /etc/ratatoskr/offhost-drill.env
sudo docker run -d --name ratatoskr-offhost-drill-postgres --restart unless-stopped \
  -e POSTGRES_PASSWORD=CHANGE-ME postgres:17
sudo cp deploy/backup/ratatoskr-offhost-drill.{service,timer} /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ratatoskr-offhost-drill.timer
```

The weekly Sunday timer verifies Saturday's UTC object. It downloads, decrypts, restores into an
ICU-collated scratch database, checks Platform schemas and constraints, drops that database, and
prints exactly `PASS: off-host restore drill` only after cleanup. Any failing stage prints
`FAIL: <stage>` and returns nonzero. Test it once before relying on the timer:

```bash
sudo sh -c 'set -a; . /etc/ratatoskr/offhost-drill.env; set +a; \
  /usr/local/bin/ratatoskr-offhost-drill.sh --dry-run'
sudo systemctl start ratatoskr-offhost-drill.service
sudo systemctl status ratatoskr-offhost-drill.service --no-pager
```

## Restore a replacement board from off-host copy only

This procedure deliberately uses neither the failed board's NVMe nor `/mnt/backup`. It assumes a
fresh board, a standard Platform release installation, a recovery-only S3 `GetObject` credential,
and the off-board age identity. Do not perform the extraction commands on a live board: the
configuration archive overwrites `/etc` paths by design.

1. Install the base OS dependencies and Platform release by the normal deployment procedure, but do
   **not** start Platform services. Install `age`, AWS CLI v2, Docker/PostgreSQL 17 and create
   `/etc/ratatoskr` plus the NVMe directories.
2. Export the recovery S3 environment and copy the age identity from its off-board custody to a
   root-only temporary path. Select the recovery date and download only the remote objects:

   ```bash
   day=YYYY-MM-DD
   base="${RATATOSKR_OFFHOST_PREFIX:-ratatoskr-platform}/$day"
   config_key=$(aws s3api list-objects-v2 --bucket "$RATATOSKR_OFFHOST_BUCKET" \
     --prefix "$base/configuration/" --query 'Contents[0].Key' --output text)
   dump_key=$(aws s3api list-objects-v2 --bucket "$RATATOSKR_OFFHOST_BUCKET" \
     --prefix "$base/postgresql/" --query 'Contents[0].Key' --output text)
   test "$config_key" != None && test "$dump_key" != None
   aws s3 cp "s3://$RATATOSKR_OFFHOST_BUCKET/$config_key" /root/configuration.tar.age
   aws s3 cp "s3://$RATATOSKR_OFFHOST_BUCKET/$dump_key" /root/ratatoskr.dump.age
   age --decrypt --identity /root/offhost-recovery.agekey --output /root/configuration.tar \
     /root/configuration.tar.age
   sudo tar --extract --file /root/configuration.tar --directory /
   age --decrypt --identity /root/offhost-recovery.agekey --output /root/ratatoskr.dump \
     /root/ratatoskr.dump.age
   ```

3. Recreate the NATS nkey and its non-backed-up seed following `deploy/nats/README.md`; restore the
   three Platform service units and environment files from the configuration archive before any
   Platform service starts. Recreate the PostgreSQL database/roles with
   `deploy/postgres/01-database-and-roles.sql`, then restore the remote dump and grants:

   ```bash
   docker exec -i shared-postgres psql -U postgres -d postgres < deploy/postgres/01-database-and-roles.sql
   sudo sh -c 'docker exec -i shared-postgres pg_restore -U postgres --dbname=ratatoskr \
     --no-owner --exit-on-error < /root/ratatoskr.dump'
   docker exec -i shared-postgres psql -U postgres -d ratatoskr < deploy/postgres/02-grants.sql
   sudo systemctl enable --now ratatoskr-edge ratatoskr-ingest ratatoskr-scheduler
   ```

4. Remove the temporary decrypted files and identity when the restore is checked. The remote Borg
   export is an additional recovery artifact; Platform state is restored from the database dump and
   configuration archive, not from another service's blobs.

Continuous WAL shipping is intentionally deferred. It would change this bounded daily recovery point
into a continuously credentialed remote transport with its own archive retention, monitoring,
ordering, and point-in-time recovery contract. It deserves a separate design; it is not a hidden
fallback in this daily-copy system.
