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
| PostgreSQL data | `/mnt/nvme/shared-postgres/data` | the `shared-postgres` container, not Platform |
| `JetStream` store | `/mnt/nvme/ratatoskr/nats` | `deploy/nats/ratatoskr.conf` |
| Service logs | `/mnt/nvme/ratatoskr/logs` | the units, rotated by `deploy/logrotate/ratatoskr` |
| Database dumps | `/mnt/nvme/backups/ratatoskr` | daily local custom dumps and transient off-host staging |
| Local Borg copy | `/mnt/backup/borg` | a second volume on the SAME machine, never an off-host replica |
| Off-host copies | configured S3-compatible bucket | age-encrypted dump, Borg export, and configuration recovery set |
| AI archive staging | `/mnt/nvme/ratatoskr/archive-staging` | `ratatoskr-edge`, mode `0700`, provisioned by `deploy/tmpfiles.d/ratatoskr-platform.conf` |

Install `tmpfiles.d/ratatoskr-platform.conf` as
`/etc/tmpfiles.d/ratatoskr-platform.conf` and run
`systemd-tmpfiles --create /etc/tmpfiles.d/ratatoskr-platform.conf` before starting
`ratatoskr-edge`.

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

Edge also forwards to loopback upstreams, whose addresses are allocated in the workspace port table and
configured in `systemd/edge.conf.example`. Each is bound by the owning service's own listener rather
than its operator listener: Knowledge's API listener on 8091, GitHub's on 8092, and the ChatGPT and
Claude receipt listeners on 8096 and 8097. Vault (8093), social (8094) and AI (8095) have no
implementing service yet, so nothing listens there and Edge reports their capability stale on
purpose. That is the truthful answer, not a fault to silence.

`8081` is deliberately unused: it is held by another process, which is why `ratatoskr-ingest` carries
no compiled default public port and rule V1 refuses to start it until an operator names one.

The operator listeners bind `0.0.0.0`, and that is a decision rather than an oversight. The metrics
stack is a container on the Docker bridge, and a host loopback port is not reachable from there. The
exposure is bounded by `IPAddressAllow=` in each unit — loopback, the bridge, and Tailscale — which
is enforcement that lives in this repository rather than host firewall state a reflash resets.

## Installing

In this order. Steps 3 and 5 are separated by step 4 because `schema.sql` creates the schemas, and a
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

# 2. The binaries. Every commit on main publishes them, after the gate and the arm64 smoke test, as
#    ghcr.io/po4yka/ratatoskr-platform:sha-<commit> with SHA256SUMS and a provenance attestation,
#    and ratatoskr-workspace's deploy/ansible playbook installs exactly that. By hand, verify on a
#    machine with `oras` and `gh` and copy the three files over; never build on the host:
#      oras pull "ghcr.io/po4yka/ratatoskr-platform:sha-$COMMIT" -o out
#      gh attestation verify "oci://ghcr.io/po4yka/ratatoskr-platform:sha-$COMMIT" \
#        --repo po4yka/ratatoskr-platform --source-digest "$COMMIT" \
#        --signer-workflow po4yka/ratatoskr-workspace/.github/workflows/reusable-release-artifact.yml
#      (cd out && sha256sum -c SHA256SUMS)
#    The fallback is the same export stage built locally:
#      docker buildx build --platform linux/arm64 --target export \
#        --build-arg RATATOSKR_GIT_SHA="$(git rev-parse HEAD)" --output type=local,dest=out .
sudo install -o root -g root -m 0755 out/ratatoskr-edge out/ratatoskr-ingest out/ratatoskr-scheduler \
  /usr/local/bin/

# 3. The database and the roles, then the passwords. PostgreSQL is a CONTAINER on this host
#    (`shared-postgres`), so every psql below enters it; there is no `postgres` user on the host and
#    no host client of a matching major version.
docker exec -i shared-postgres psql -U postgres -d postgres < deploy/postgres/01-database-and-roles.sql
# Set each password through stdin, never as an argument: argv is visible to every user in `ps`.
printf "alter role ratatoskr_edge password '%s';\n" "$(openssl rand -base64 24 | tr -d '=+/')" \
  | docker exec -i shared-postgres psql -U postgres -q -d postgres      # and ingest, and scheduler
# Every other service's role comes from its own repository, after step 6 (see "PostgreSQL: one
# database, one role per service" below).

# 4. The bus credential, then the server. deploy/nats/README.md generates the thirteen pairs (one per
#    identity), lists each seed file and the variable that names it, and gives the mandatory order:
#    reload NATS, restart Edge (it provisions every durable and the completion bucket), then start
#    the other services. The public half goes into ratatoskr.conf and the seed into a file only its
#    service can read.
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
# Edit each one: the CHANGE-ME passwords, edge's assertion key (see "The Telegram assertion key"
# below) and its OAuth completion URL.
sudo cp deploy/systemd/ratatoskr-*.service deploy/systemd/ratatoskr.target /etc/systemd/system/
sudo cp deploy/logrotate/ratatoskr /etc/logrotate.d/ratatoskr
sudo systemctl daemon-reload
# Every Ratatoskr unit is WantedBy=ratatoskr.target, so the target is what boot starts.
sudo systemctl enable ratatoskr.target

# 6. Edge first, because it applies `schema.sql`.
sudo systemctl enable --now ratatoskr-edge
docker exec -i shared-postgres psql -U postgres -d ratatoskr < deploy/postgres/02-grants.sql
sudo systemctl enable --now ratatoskr-ingest ratatoskr-scheduler

# 7. The firewall. This host runs ufw with `INPUT policy DROP`, so the operator listeners are
#    reachable from the metrics stack only with an explicit rule — without it the scrape times out
#    rather than being refused, which reads like a service that is down. Narrow: the monitoring
#    bridge and the five operator ports of ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md, which
#    are these three and Telegram's 9467 and 9468, nothing else. This is the same rule as that
#    document's; change both together. Check the subnet first, it is per-installation:
#    `docker network inspect monitoring_default --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}'`
sudo ufw allow proto tcp from 172.19.0.0/16 to any port 9464:9468 \
  comment 'ratatoskr operator listeners, from the monitoring bridge'

# 8. Metrics, only once the services are actually running: a scrape target for a service that is not
#    deployed is a permanently failing target. See deploy/monitoring/promscrape.ratatoskr.yml for
#    the one change this needs in the monitoring stack's own compose file.
cat deploy/monitoring/promscrape.ratatoskr.yml >> /home/po4yka/monitoring/promscrape.yml
docker kill -s HUP victoriametrics

# 9. Backup and off-host recovery. deploy/backup/README.md owns package installation, root-only
#    S3/age environment files, lifecycle policy, the verifier host and the remote-only restore drill.
#    Run its mandatory dry-runs and the verifier drill before considering recovery configured.
```

Every step is re-runnable. `create database` reports that the database exists and is skipped; every
other statement is a `grant`, an `alter role`, or an install.

**The deployment stops and restarts as one thing.** `sudo systemctl stop ratatoskr.target` stops
every Ratatoskr unit on the host, and `restart` restarts them; each unit, Platform's three and those
of the extractor and Telegram, carries `PartOf=ratatoskr.target`. The containers are not members, so
PostgreSQL and NATS stay up. A unit that fails does not stop the target or its siblings.

**A schema change reaches this database only if the database is recreated.** `ratatoskr-edge`
applies `schema.sql` when the `identity` schema is absent, and skips the apply when the schema is
already there. An edit to the file therefore changes nothing on a host that has already run step 6.

## PostgreSQL: one database, one role per service

Ratatoskr has ONE database, `ratatoskr`, created by `postgres/01-database-and-roles.sql` with the ICU collation, and every service connects to it as its own role, `ratatoskr_<service>`, with its own schema inside it. Platform owns `ratatoskr_edge`, `ratatoskr_ingest` and `ratatoskr_scheduler`; every other repository provisions its role with a checked-in `deploy/postgres/role.sql` of its own and names it in its example as `postgres://ratatoskr_<service>:CHANGE-ME@127.0.0.1:5432/ratatoskr`.

| Role | Provisioned by | Schema it creates |
|---|---|---|
| `ratatoskr_edge`, `ratatoskr_ingest`, `ratatoskr_scheduler` | this repository, `postgres/01-database-and-roles.sql` and `postgres/02-grants.sql` | `identity`, `operations`, `platform_ingest` |
| `ratatoskr_chatgpt` | `ratatoskr-chatgpt`, `deploy/postgres/role.sql` | `chatgpt_archive` |
| `ratatoskr_claude` | `ratatoskr-claude`, `deploy/postgres/role.sql` | `claude_archive` |
| `ratatoskr_knowledge` | `ratatoskr-knowledge`, `deploy/postgres/role.sql` (also runs `create extension vector`, which only an administrator may) | `knowledge` |
| `ratatoskr_channel_digests` | `ratatoskr-channel-digests`, `deploy/postgres/role.sql` | `channel_digests` |
| `ratatoskr_telegram` (webhook and dispatcher share it) | `ratatoskr-telegram`, `deploy/postgres/role.sql` | `telegram` |
| `ratatoskr_x`, `ratatoskr_instagram`, `ratatoskr_threads` | the social repositories, `deploy/postgres/role.sql` | `x_archive`, `instagram_archive`, `threads_archive` |
| `ratatoskr_vault`, `ratatoskr_github` | `ratatoskr-vault` and `ratatoskr-github`, `deploy/postgres/role.sql` | `git_vault`, `github_catalog` |
| `ratatoskr_extractor` | `ratatoskr-extractor`, `deploy/postgres/role.sql` | `extractor` |

The order matters, and each step is run by a PostgreSQL administrator:

1. `postgres/01-database-and-roles.sql`: the database, the three Platform roles and the revoke of `connect` from public.
2. The first start of `ratatoskr-edge`, which applies `schema.sql` and so creates the `identity` and `operations` schemas.
3. `postgres/02-grants.sql`, which can name those schemas now that they exist.
4. Each other service's own `deploy/postgres/role.sql`, before that service's first start. A role is created without a password; set it with `\password` (never as an argument) and put it in the service's environment file. Knowledge's file must be run by an administrator because `vector` is not a trusted extension: without it Knowledge's schema apply fails with `permission denied to create extension "vector"`.

A service that starts before its role exists fails to connect and exits; systemd restarts it, so the symptom of a wrong order is a restart loop that names the missing role, not corruption.

## The Telegram assertion key

`POST /v1/sessions/telegram` verifies an identity assertion that `ratatoskr-telegram` signs with an Ed25519 key. Telegram holds the PRIVATE seed (a 64-hex-character file, `RATATOSKR__PLATFORM__ASSERTION_SIGNING_KEY_FILE`) and Edge holds only the PUBLIC key, as base64 with padding, in `RATATOSKR__IDENTITY__ASSERTION_KEY` (ADR-0011, rule V14). `edge.conf.example` ships that line active with the placeholder `CHANGE-ME`, which the loader refuses, so an operator who skips this procedure gets a startup error naming `identity.assertion_key` rather than a Mini App that silently answers 401. On the host:

```bash
# 1. Generate the seed with Telegram's own command. It writes 64 lowercase hex characters, mode 0600,
#    refuses to overwrite an existing file, and prints the PUBLIC key only.
sudo ratatoskr-telegram-webhook assertion-key generate /etc/ratatoskr/secrets/telegram-platform-signing-key

# 2. The seed is read by Telegram, so give it to Telegram's group.
sudo chown root:ratatoskr-telegram-secrets /etc/ratatoskr/secrets/telegram-platform-signing-key
sudo chmod 0640 /etc/ratatoskr/secrets/telegram-platform-signing-key

# 3. Derive the public key again whenever it is needed (it prints nothing else on stdout) and paste
#    it into the RATATOSKR__IDENTITY__ASSERTION_KEY line of /etc/ratatoskr/edge.conf.
sudo ratatoskr-telegram-webhook assertion-key public /etc/ratatoskr/secrets/telegram-platform-signing-key
```

The known-answer check for both ends is RFC 8032 section 7.1, test 1: the seed `9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60` has the public key `11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=`. Do not use that pair on a host. After editing `edge.conf`, `ratatoskr-edge check-config` accepts the line and `GET /v1/capabilities` lists `telegram.mini_app`.

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

`ratatoskr-scheduler` publishes from `operations.schedules`. Domain services register their own
named cron schedules by publishing `cmd.platform.schedule.registration_requested.v1`; Edge consumes
that command through its durable inbox and scheduler reads the reconciled row. The fleet configures
`RATATOSKR__SCHEDULING__ALLOWED_REGISTRARS` on Edge. This is an envelope-level interim allowlist,
not cryptographic service authentication: provision a distinct NATS identity per service, restricted
to this exact subject, before relying on it as an authorization boundary.

The allowlist names two registrars: `ratatoskr-github` (allowlisted, no producer wired) and
`ratatoskr-channel-digests`, which registers its daily digest (`daily-digest`, cron from its own
`RATATOSKR__SCHEDULE__CRON`, default `0 6 * * *`) when its worker starts. The registration is the
contract command `platform.schedule.registration_requested.v1`, and Edge refuses a document that is
not exactly that envelope or whose producer is not the `service_name` it names. **Operator
prerequisite:** the channel-digests worker's `RATATOSKR__SCHEDULE__OWNER_USER_ID` must be a user that
already exists in `identity.users`. Platform has no system principal, and every occurrence creates an
operation owned by that user, who is the only one who can read it. The schema has no foreign key to
enforce this (`DATA_MODEL.md` forbids cross-schema references), so a schedule owned by an id that is
not a user registers and publishes without complaint, and its operations belong to nobody.

```sql
-- docker exec -i shared-postgres psql -U postgres -d ratatoskr
select service_name, name, owner_user_id, next_due_at, enabled, last_outcome
  from operations.schedule_status
 order by service_name, name;
```

- Cron uses five UTC fields: minute, hour, day of month, month, day of week. Registration computes
  its first `next_due_at` strictly after receipt and does not backfill. An edit retains an already
  due occurrence, so its deterministic ID can be published exactly once.
- `command_type` is the type, never the subject. `cmd.` is added by the publisher, and a CHECK
  refuses a value that already carries it.

Watch it with `platform_scheduler_drift_seconds{schedule}` and
`platform_scheduler_occurrences_total{schedule,outcome}`. A `suppressed` count above zero means
something is republishing an occurrence that already happened.

## Routes added by the fleet integration fixes

- `POST /v1/captures/blobs`: a Telegram Mini App session submits a PDF the Telegram service stored,
  by reference. Nothing to deploy on Edge; the extractor must be able to read the Telegram blob root
  (`ratatoskr-workspace` deployment notes).
- `PUT /v1/channel-digests/subscriptions/{channel_username}`, `POST /v1/channel-digests/runs` and the
  three `GET /v1/channel-digests/...` reads. The reads need `RATATOSKR__CHANNEL_DIGESTS__LISTENER`
  (loopback, `127.0.0.1:8098`) and `RATATOSKR__CHANNEL_DIGESTS__SERVICE_SECRET` in
  `deploy/systemd/edge.conf.example`; without them the reads answer `503` and the commands still work.
- AI archive uploads: chunks up to 16 MiB are accepted, so keep
  `RATATOSKR__PUBLIC__MAX_BODY_BYTES` at least that large (the shipped value is 100 MiB; a deployment
  on the compiled 1 MiB default still cannot take chunks above 1 MiB). The archive ceiling is
  `RATATOSKR__ARCHIVE_STAGING__MAX_ARCHIVE_BYTES` (default 2 GiB); staging needs about twice that
  in free space on the staging mount per in-flight archive.
- The bus carries the owner's stream policy (XR-021 R5-11 to R5-13). `ratatoskr_events` keeps
  dropping its oldest message at seven days or 1 GiB, and three things make that safe to run:
  Knowledge acknowledges every deletion fact on `evt.knowledge.deletion.acknowledged.v1`, read by the
  five producers through their own `ratatoskr_{chatgpt,claude,x,instagram,threads}_deletion_acks`
  durables (Edge provisions them; reload NATS and restart Edge before the services); ChatGPT and
  Claude can ask Edge to erase the stream messages they published for a deleted owner by sending
  `cmd.platform.stream_erasure.requested.v1`, which Edge's `platform_stream_erasure` consumer honours
  only for a message that carries the requesting producer's name and the named owner and is not a
  tombstone (it writes one content-free row per command to `operations.stream_erasure_audit` and
  counts `platform_stream_erasure_total{outcome}`); and Edge's observer exports the lag of every
  durable and the state of both streams, which the alert rules in
  `deploy/monitoring/ratatoskr-bus-alerts.rules.yml` watch. The durable table and the ACL are in
  `deploy/nats/README.md`.
- AI archive staging is reaped. When an archive receiver is configured, `ratatoskr-edge` runs a
  reaper every `RATATOSKR__ARCHIVE_STAGING__SWEEP_INTERVAL_SECONDS` (rule V23, 30 to 3600, default
  300). A sweep handles at most 200 transfer sessions and 200 orphans, oldest first. It removes the
  directory of a transfer and then deletes its row when the session `failed` or `finalized`, when it
  expired, or when its operation has been terminal for more than ten minutes, so a removal that
  fails leaves the row for the next sweep. It also removes a directory named `rst_*` that no row
  owns and `.readiness-*` files, once they are older than an hour. It never follows a symbolic link
  and never touches a name outside the token grammar. The same loop re-checks that the root is
  writable and that the volume has at least twice `MAX_ARCHIVE_BYTES` free, and Edge's
  `ArchiveStaging` readiness follows the answer, so a full or unmounted volume refuses new archives
  within one interval instead of at the next restart. Opening an upload again after a digest
  mismatch, a permanent refusal by the receiver or an expiry returns a new token (`201`); an
  `open` session that has not expired and a `finalized` one return their own. The series are
  `platform_archive_staging_removed_total{reason}` (`expired`, `failed`, `finalized`, `terminal`,
  `orphan`), `platform_archive_staging_directories` and
  `platform_archive_staging_oldest_directory_age_seconds`. `deploy/tmpfiles.d/ratatoskr-platform.conf`
  keeps the age field `-` on purpose: the reaper owns removal, and a systemd age sweep would delete
  the directory of a session that is still being resumed.

## What the services now report

Every row of `ARCHITECTURE.md` S16 has a publication point, and
`platform_telemetry::metrics::ALL` is the whole list. The ones worth a rule first:

| Series | What a non-zero value means |
|---|---|
| `platform_outbox_dead_lettered` | work a client was told had been accepted and that nobody delivered. Never expected above zero |
| `platform_outbox_oldest_pending_age_seconds` | the publisher is not draining. With edge down, everything the other two roles accept lands here |
| `platform_inbox_unprocessed` | a handler is failing after claiming a message |
| `platform_operations_oldest_unterminated_age_seconds` | an operation nobody finished. The reaper (ADR-0014) terminates the ones past their window; a value persistently above the window means the reaper is not running or the window is wrong for how long work here actually takes |
| `platform_scheduler_occurrences_total{outcome="skipped"}` | the scheduler is not keeping up with its own schedules |
| `platform_capability_available` | a capability the deployment is advertising as unavailable |
| `platform_bus_consumer_oldest_unacked_age_seconds{stream,durable}` | a consumer is behind: the age of the first message after its acknowledgement floor, 0 when it has caught up. `ratatoskr_events` drops its oldest messages after seven days, so a value in days is facts about to be lost. Sampled by Edge's observer every 15 s, with `platform_bus_consumer_pending`, `platform_bus_consumer_ack_pending` and `platform_bus_consumer_redelivered` |
| `platform_bus_stream_bytes{stream}` against `platform_bus_stream_max_bytes{stream}` | a stream near its 1 GiB cap; the events stream then drops its oldest messages early. `platform_bus_stream_messages` and `platform_bus_stream_oldest_message_age_seconds` complete the picture |

## What is still missing

Named here rather than left to be discovered:

- **A running alert engine.** Alertmanager on this host reaches a person, and the rules now exist:
  `deploy/monitoring/ratatoskr-bus-alerts.rules.yml` holds the six bus alerts (Knowledge consumer lag
  at one hour and at two days, Knowledge backlog, the events stream near its byte cap, a redelivery
  storm, and a deletion fact that Knowledge has not acknowledged for a day), with the thresholds and
  the reasoning for them written beside each. The owner chooses the engine; vmalert and Prometheus
  both load this file. Nothing in this repository starts one, so the rules watch nothing until the
  engine is pointed at the file. `RatatoskrUnacknowledgedTombstones` also needs the five deletion
  producers' operator ports as scrape targets, which stay host-only until the owner opens them to the
  monitoring bridge; the targets are in `promscrape.ratatoskr.yml`, commented out.
- **Operation history retention.** The sweep deliberately does not touch `operations.operations`:
  how long a user's history is kept is a product decision, and no milestone owns it.
