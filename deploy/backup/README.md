# Backup, and the restore that proves it

A backup nobody has restored is a file. This directory is a dump job, its timer, and the rehearsal
that turns the file into a backup.

## What is copied, and what is not

The dump is `pg_dump --format=custom` of the `ratatoskr` database. That is the whole of Platform's
durable state: `identity`, `operations` and `platform_ingest`. Nothing else here holds anything that
survives a restart —

- **`JetStream`'s store is deliberately not backed up.** A command in it is a copy of an
  `operations.outbox` row that has not been marked published, and an event in it has already been
  applied to the projection or is still redeliverable from the producer. Restoring a stream from a
  backup would replay work the database says is finished, which is worse than losing it. If the
  store is lost, delete the streams and let `ratatoskr-edge` recreate them; the outbox republishes.
- **The nkey seed and the environment files are not backed up here.** They are credentials, and a
  credential in the same archive as the database it opens is one theft rather than two. They are
  regenerated (`deploy/nats/README.md`) or re-entered.

## Where it goes, and the ordering that matters

`/mnt/nvme/backups/ratatoskr`, fourteen dumps, on the NVMe device. The host's `borg` job then copies
`/mnt/nvme` to `/mnt/backup`.

**The timer runs at 02:30 because borg runs at 03:00.** Confirmed on the machine at milestone 10:
`borgmatic` is a `0 3 * * *` line in root's crontab, not a systemd timer, so `systemctl list-timers`
does not show it and looking there is how somebody concludes there is no borg. A dump written after borg has already run is
not copied anywhere until the following night, so the effective recovery point is two days old
rather than one — the ordering `ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md` records as wrong.
Check `systemctl list-timers` before trusting the number: if borg has moved, this must move with it.

**`/mnt/backup` is a second volume on the same machine.** It survives a disk failure and does not
survive losing the board — a fire, a theft, a power supply that takes the whole thing with it. There
is no off-host copy of anything in this system today, and this file is not the place that changes;
it is the place that says so. Until one exists, the honest statement of the recovery point is: one
day for a disk failure, and everything for the loss of the machine.

## Installing

```bash
sudo install -m 0755 deploy/backup/ratatoskr-dump.sh /usr/local/bin/ratatoskr-dump.sh
sudo install -d -m 0700 /mnt/nvme/backups/ratatoskr
sudo cp deploy/backup/ratatoskr-backup.{service,timer} /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ratatoskr-backup.timer
sudo systemctl start ratatoskr-backup.service   # once, now, rather than waiting for 02:30
```

## The rehearsal

Run this after installing, and again after any migration that changes a constraint. It restores into
a scratch database on the same cluster, so it needs no second machine and touches nothing live.

PostgreSQL is a container on this host, so every command below enters it. `--jobs` is deliberately
absent: `pg_restore` refuses a parallel restore from standard input, and standard input is how a
dump on the host reaches a client inside the container. It is a small database and the serial
restore takes seconds.

```bash
# 1. The newest dump, and the fact that it is readable at all.
dump=$(sudo sh -c 'ls -1t /mnt/nvme/backups/ratatoskr/ratatoskr-*.dump | head -1')
sudo sh -c "docker exec -i shared-postgres pg_restore --list < $dump" | tail -5

# 2. A scratch database with the SAME locale. Restoring into a libc-collated database rebuilds every
#    text index under a different collation, which is a restore that succeeds and is wrong.
docker exec shared-postgres createdb -U postgres ratatoskr_rehearsal \
  --template=template0 --locale-provider=icu --icu-locale=und-x-icu --encoding=UTF8

# 3. The restore. `--exit-on-error` because a restore that reports success after skipping a
#    constraint is the failure this rehearsal exists to catch.
sudo sh -c "docker exec -i shared-postgres pg_restore -U postgres --dbname=ratatoskr_rehearsal \
  --no-owner --exit-on-error < $dump"

# 4. What must be true afterwards. Row counts are not the check — a restore that dropped a UNIQUE
#    index would keep every row and lose the guarantee. The check is that the constraints are back.
docker exec shared-postgres psql -U postgres -d ratatoskr_rehearsal -Atc \
  "select count(*) from pg_constraint where connamespace::regnamespace::text
     in ('identity','operations','platform_ingest')"
docker exec shared-postgres psql -U postgres -d ratatoskr_rehearsal -Atc \
  "select count(*) from pg_indexes where schemaname
     in ('identity','operations','platform_ingest')"
# `i` and `und-x-icu`, not `c`. A restore into a libc-collated database succeeds and rebuilds every
# text index under a collation the deployment does not use, which is the failure mode that ends with
# one external account mapping to two internal users.
docker exec shared-postgres psql -U postgres -Atc \
  "select datlocprovider, datlocale from pg_database where datname='ratatoskr_rehearsal'"

# 5. Compare with the live database. Equal, or the restore is not one.
docker exec shared-postgres psql -U postgres -d ratatoskr -Atc \
  "select count(*) from pg_constraint where connamespace::regnamespace::text
     in ('identity','operations','platform_ingest')"

# 6. Clean up. The rehearsal database is not a second copy of anything and must not become one.
docker exec shared-postgres dropdb -U postgres ratatoskr_rehearsal
```

If step 3 fails, the dump is not a backup and the failure is today's problem rather than the problem
of whichever day the database is gone.

Rehearsed twice: on a development PostgreSQL 17 before this file was committed, and at milestone 10
**on the deployment target itself** — 141 constraints, 58 indexes and three operations restored into
an ICU-collated scratch database on the host's own cluster, matching live exactly. `pg_database.datlocale` is the column that reports the locale on 17; it was
`daticulocale` on 15 and 16, so a rehearsal script copied from an older runbook fails at step 4 with
a message about a missing column rather than about the backup.
