# The deployment profile

One machine, three systemd units, one PostgreSQL database inside a cluster that already exists, and
one NATS credential. This directory is the whole profile; nothing in it is generated and nothing in
it is optional.

The machine itself — the board, its storage devices, what else runs on it, and which ports are
already taken — is described in `ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`, which is the
document to change first. This one says what Platform puts on it.

**Not Kubernetes, not Compose, and there will not be a second host.** ADR-0010 records why every
lock, lease and deduplication control is kept anyway, and ADR-0013 records the profile decisions
below.

## Storage

Absolute paths, never named volumes: a named Docker volume lands wherever `DockerRootDir` points,
which is host state no repository owns and which a reflash resets to the boot device.

| Purpose | Path | Owner |
|---|---|---|
| PostgreSQL data | `/mnt/nvme/ratatoskr/postgres` | the cluster, not Platform |
| `JetStream` store | `/mnt/nvme/ratatoskr/nats` | `deploy/nats/ratatoskr.conf` |
| Service logs | `/mnt/nvme/ratatoskr/logs` | the units, rotated by `deploy/logrotate/ratatoskr` |
| Database dumps | `/mnt/nvme/backups/ratatoskr` | not yet written; see "What is still missing" |
| Off-host copies | `/mnt/backup/borg` | a second volume on the SAME machine, never an off-host replica |

**PostgreSQL and `JetStream` must never write to the boot device.** Their pattern is small
synchronous fsyncs, which is the worst case for flash wear, and the SD card that wears out takes the
root filesystem with it.

## Ports

A port on this host is an **allocation**, not a default. Never answer a bind failure by widening a
bind to `0.0.0.0`.

| Port | Owner | Reachable from |
|---|---|---|
| 8080 | `ratatoskr-edge` public API | `cloudflared` on this host |
| 8181 | `ratatoskr-ingest` webhook adapter | `cloudflared` on this host |
| 9464 / 9465 / 9466 | edge / ingest / scheduler operator listener | the Docker bridge and Tailscale |
| 4222 | NATS | this host |
| 5432 | PostgreSQL | this host |

`8081` is deliberately unused: it is held by another process, which is why `ratatoskr-ingest` carries
no compiled default public port and rule V1 refuses to start it until an operator names one.

The operator listeners bind `0.0.0.0`, and that is a decision rather than an oversight. The metrics
stack is a container on the Docker bridge, and a host loopback port is not reachable from there. The
exposure is bounded by `IPAddressAllow=` in each unit — loopback, the bridge, and Tailscale — which
is enforcement that lives in this repository rather than host firewall state a reflash resets.

## Installing

In this order. Steps 3 and 5 are separated by step 4 because a migration creates the schemas, and a
grant cannot name a schema that does not exist yet.

```bash
# 1. Users and directories. Each service gets its OWN group as well as the shared one: the shared
#    group is what makes /mnt/nvme/ratatoskr/logs writable by all three, and the per-role group is
#    what makes /etc/ratatoskr/<role>.conf readable by ONE of them. With only a shared group, every
#    service can read every other service's database password.
sudo groupadd --system ratatoskr
for role in edge ingest scheduler; do
  sudo useradd --system --user-group --no-create-home --shell /usr/sbin/nologin "ratatoskr-$role"
  sudo usermod -aG ratatoskr "ratatoskr-$role"
done
sudo useradd --system --user-group --no-create-home --shell /usr/sbin/nologin ratatoskr-nats
sudo install -d -m 0750 -o root -g ratatoskr /etc/ratatoskr
sudo install -d -m 0770 -o root -g ratatoskr /mnt/nvme/ratatoskr/logs
sudo install -d -m 0700 -o ratatoskr-nats -g ratatoskr-nats /mnt/nvme/ratatoskr/nats
sudo install -d -m 0700 /mnt/nvme/backups/ratatoskr

# 2. The binaries. Built for linux/arm64 from this repository's Dockerfile and copied out of the
#    image; milestone 10 is where that path is exercised on the target end to end.
docker buildx build --platform linux/arm64 \
  --build-arg RATATOSKR_GIT_SHA="$(git rev-parse HEAD)" -t ratatoskr-platform:deploy .
id=$(docker create ratatoskr-platform:deploy)
for role in edge ingest scheduler; do
  docker cp "$id:/usr/local/bin/ratatoskr-$role" - | sudo tar -x -C /usr/local/bin
done
docker rm "$id"
sudo chown root:root /usr/local/bin/ratatoskr-*
sudo chmod 0755 /usr/local/bin/ratatoskr-*

# 3. The database and the roles, then the passwords. PostgreSQL is a CONTAINER on this host
#    (`shared-postgres`), so every psql below enters it; there is no `postgres` user on the host and
#    no host client of a matching major version.
docker exec -i shared-postgres psql -U postgres -d postgres < deploy/postgres/01-database-and-roles.sql
# Set each password through stdin, never as an argument: argv is visible to every user in `ps`.
printf "alter role ratatoskr_edge password '%s';\n" "$(openssl rand -base64 24 | tr -d '=+/')" \
  | docker exec -i shared-postgres psql -U postgres -q -d postgres      # and ingest, and scheduler

# 4. The bus credential, then the server. deploy/nats/README.md generates the pair; the public half
#    goes into ratatoskr.conf and the seed into a file only edge can read.
sudo install -d -m 0755 /etc/nats
sudo cp deploy/nats/ratatoskr.conf /etc/nats/ratatoskr.conf   # with the public nkey substituted
sudo install -m 0644 deploy/nats/compose.yaml /etc/nats/compose.yaml
printf 'RATATOSKR_NATS_UID=%s\nRATATOSKR_NATS_GID=%s\n' \
  "$(id -u ratatoskr-nats)" "$(id -g ratatoskr-nats)" | sudo tee /etc/nats/.env > /dev/null
sudo docker compose --env-file /etc/nats/.env -f /etc/nats/compose.yaml up -d

# 5. The units and their environment files.
for role in edge ingest scheduler; do
  sudo install -m 0640 -o root -g "ratatoskr-$role" \
    "deploy/systemd/$role.conf.example" "/etc/ratatoskr/$role.conf"
done
# Edit each one: the CHANGE-ME passwords, and edge's assertion key and OAuth completion URL.
sudo cp deploy/systemd/ratatoskr-*.service /etc/systemd/system/
sudo cp deploy/logrotate/ratatoskr /etc/logrotate.d/ratatoskr
sudo systemctl daemon-reload

# 6. Edge first, because it applies the migrations.
sudo systemctl enable --now ratatoskr-edge
docker exec -i shared-postgres psql -U postgres -d ratatoskr < deploy/postgres/02-grants.sql
sudo systemctl enable --now ratatoskr-ingest ratatoskr-scheduler

# 7. The firewall. This host runs ufw with `INPUT policy DROP`, so the operator listeners are
#    reachable from the metrics stack only with an explicit rule — without it the scrape times out
#    rather than being refused, which reads like a service that is down. Narrow: the monitoring
#    bridge and those three ports, nothing else. Check the subnet first, it is per-installation:
#    `docker network inspect monitoring_default --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}'`
sudo ufw allow proto tcp from 172.19.0.0/16 to any port 9464:9466 \
  comment 'ratatoskr operator listeners, from the monitoring bridge'

# 8. Metrics, only once the services are actually running: a scrape target for a service that is not
#    deployed is a permanently failing target. See deploy/monitoring/promscrape.ratatoskr.yml for
#    the one change this needs in the monitoring stack's own compose file.
cat deploy/monitoring/promscrape.ratatoskr.yml >> /home/po4yka/monitoring/promscrape.yml
docker kill -s HUP victoriametrics

# 9. Backup. deploy/backup/README.md has the detail and the restore rehearsal; run the rehearsal
#    once now rather than on the day it is needed.
sudo install -m 0755 deploy/backup/ratatoskr-dump.sh /usr/local/bin/ratatoskr-dump.sh
sudo cp deploy/backup/ratatoskr-backup.{service,timer} /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ratatoskr-backup.timer
sudo systemctl start ratatoskr-backup.service
```

Every step is re-runnable. `create database` reports that the database exists and is skipped; every
other statement is a `grant`, an `alter role`, or an install.

## Checking it

```bash
# Configuration, without starting anything. This is what each unit runs as ExecStartPre, and it is
# where a missing or UNREADABLE nkey seed is caught (rules V13 and V16). Run it through systemd, as
# the service user: a check run as root reads files the service cannot, which is how a permission
# problem passes validation and fails at startup.
sudo systemd-run --quiet --wait --pipe --collect \
  --uid=ratatoskr-edge --property=EnvironmentFile=/etc/ratatoskr/edge.conf \
  /usr/local/bin/ratatoskr-edge check-config

# Readiness, over Tailscale or from the host.
curl -s http://127.0.0.1:9464/health/ready

# The privilege boundary. Both must be `f`.
docker exec shared-postgres psql -U postgres -d ratatoskr -Atc \
  "select has_table_privilege('ratatoskr_ingest','identity.sessions','select'),
          has_schema_privilege('ratatoskr_scheduler','identity','usage')"

# The backlog. A pending count that only grows means the pump is not running, which on this host
# means ratatoskr-edge is down — it is the only publisher.
docker exec shared-postgres psql -U postgres -d ratatoskr -Atc \
  "select count(*) filter (where published_at is null and dead_lettered_at is null),
          count(*) filter (where dead_lettered_at is not null) from operations.outbox"

# The metrics path, from where the collector actually stands. A timeout here is the firewall; a
# refusal is the service.
docker exec victoriametrics wget -q -O- --timeout=4 \
  "http://$(docker network inspect monitoring_default \
     --format '{{range .IPAM.Config}}{{.Gateway}}{{end}}'):9464/health/ready"
```

`journalctl -u ratatoskr-edge` shows systemd's lines and **not** the service's: the units write to
`/mnt/nvme/ratatoskr/logs/*.log`, because `/var/log` here is a 128 MiB log2ram tmpfs.

## Schedules

`ratatoskr-scheduler` publishes from `operations.schedules`, and there is no route that writes that
table: an operator inserts a row. Every schedule is created **disabled**, because enabling one starts
publishing commands to a domain service that may not exist yet.

```sql
-- docker exec -i shared-postgres psql -U postgres -d ratatoskr
insert into operations.schedules
    (schedule_id, name, owner_user_id, command_type, operation_kind, payload,
     interval_seconds, next_due_at, catch_up, enabled, created_at, updated_at)
values (gen_random_uuid(), 'github-sync', '<a user_id from identity.users>',
        'github.sync.requested.v1', 'github.sync', '{"account": "po4yka"}'::jsonb,
        3600, date_trunc('hour', now()) + interval '1 hour', 'catch_up', false, now(), now());

update operations.schedules set enabled = true, updated_at = now() where name = 'github-sync';
```

- `next_due_at` is the **phase** as well as the next occurrence: an interval of 86 400 anchored at
  03:00 stays at 03:00. There is no cron expression, because a parser and its dependency would buy
  only what this column already carries.
- `catch_up` is `skip` for a snapshot — ten stale snapshots are nine units of superseded work — and
  `catch_up` for an incremental synchronisation, where a gap is a hole in the data. Catch-up is
  bounded: a schedule enabled with a due time far in the past jumps to the present and reports what
  it discarded, rather than publishing a year of commands into a stream that refuses a publish when
  it is full.
- `command_type` is the type, never the subject. `cmd.` is added by the publisher, and a CHECK
  refuses a value that already carries it.

Watch it with `platform_scheduler_drift_seconds{schedule}` and
`platform_scheduler_occurrences_total{schedule,outcome}`. A `suppressed` count above zero means
something is republishing an occurrence that already happened; a `skipped` count that keeps rising
means the process is not keeping up with its own schedules.

## What the services now report

Every row of `ARCHITECTURE.md` S16 has a publication point, and
`platform_telemetry::metrics::ALL` is the whole list. The ones worth a rule first:

| Series | What a non-zero value means |
|---|---|
| `platform_outbox_dead_lettered` | work a client was told had been accepted and that nobody delivered. Never expected above zero |
| `platform_outbox_oldest_pending_age_seconds` | the publisher is not draining. With edge down, everything the other two roles accept lands here |
| `platform_inbox_unprocessed` | a handler is failing after claiming a message |
| `platform_operations_oldest_unterminated_age_seconds` | an operation nobody finished. There is no reconciler yet, so this is the only way to see one |
| `platform_scheduler_occurrences_total{outcome="skipped"}` | the scheduler is not keeping up with its own schedules |
| `platform_capability_available` | a capability the deployment is advertising as unavailable |

## What is still missing

Named here rather than left to be discovered:

- **An off-host copy.** `deploy/backup/` dumps daily to NVMe and the host's borg job copies that to
  `/mnt/backup` — a second volume on the same machine. It survives a disk failure and does not
  survive losing the board. The honest recovery point today is one day for a disk, and everything
  for the machine.
- **Alert rules.** Alertmanager on this host reaches a person, and no rule watches any of the series
  above. The metrics exist; the rules do not.
- **A `ratatoskr.target`.** The three units are enabled individually; there is no single unit to
  stop the deployment with.
- **Operation history retention.** The sweep deliberately does not touch `operations.operations`:
  how long a user's history is kept is a product decision, and no milestone owns it.
