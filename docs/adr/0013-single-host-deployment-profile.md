# ADR-0013: The single-host deployment profile

> Status: Accepted
> Date: 2026-08-19
> Milestone: 9

## Context

ADR-0010 decided that Platform runs as exactly one process per role on one host, and explicitly did
NOT decide the profile: "stream retention, consumer configuration, the NATS credential and the
database roles — it belongs to milestone 9, as ADR-0005 already reserved." Five places in the code
and the documentation deferred a decision to this ADR by name.

This is that ADR. Everything below is transcribed into `deploy/`, and the parts a binary can
contradict are pinned by `services/edge/tests/deployment_profile.rs`.

The host is described in `ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`: a Raspberry Pi 5, Debian 12
class, four cores, 15 GiB and no disk swap, an NVMe device and an SD card that must stay out of the
write path, an existing PostgreSQL 17.7 cluster, a container-resident metrics stack, `cloudflared` as
the only public path, and nine more services queued for the same board.

## Drivers

- A profile that is only prose is a profile that drifts. Ports, timeouts and stream names exist in
  the binaries as constants, and a document that repeats them is a second source of truth.
- Four of the host's properties are live failure modes rather than preferences: the 90-second default
  `TimeoutStopSec` against a 120-second shutdown ceiling, a 128 MiB `log2ram` `/var/log`, an absent
  `sd_notify`, and a start limit that latches a dependent unit into `failed` with nothing to retry
  it.
- Least privilege has to be expressible. "Separate database roles and NATS credentials" (S18) is only
  real if there is a boundary somebody can check, and only worth having if it constrains something.

## Decisions

### 1. Schedules live in `operations`, not in a fourth schema

`ARCHITECTURE.md` S4.1 recommends `identity`, `operations` and `platform_ingest`. A schedule produces
an operation and an outbox row, both of which are in `operations`, so a fourth schema would make
every scheduler transaction a cross-schema write — which `DATA_MODEL.md` prohibits — and would give
the scheduler's database role reach into two schemas rather than one.

`platform_ingest` earned its own schema for the opposite reason: it holds ingress state neither of
the others has any claim on. A schedule has no such state.

### 2. One publisher, in `ratatoskr-edge`

**`ratatoskr-ingest` and `ratatoskr-scheduler` write commands into `operations.outbox` and publish
none of them. `ratatoskr-edge` runs the only pump.**

This was already true and already recorded as undecided in three files. Deciding it makes two things
follow:

- Neither of the other two roles holds a NATS credential, opens a NATS connection, or reads
  `RATATOSKR__BUS__URL`. `crates/scheduling` has no bus dependency at all.
- Edge being down means no command leaves the host. That is a **backlog**, not a loss: the outbox is
  durable, and `deploy/README.md` gives the query that shows it. A second pump would remove the
  coupling and add a second claimant to a shared table for a failure mode the outbox already handles
  correctly.

### 3. One NATS identity, authenticated by an nkey

The bus credential is an **nkey seed in a file named by path**, not a `.creds` file and not a
password in a URL. `RATATOSKR__BUS__NKEY_SEED_PATH` is the only way to supply it; rule V13 refuses a
URL that carries user information and rule V16 refuses a relative path or a file that is not there.

An nkey rather than `.creds` because a `.creds` file carries its permissions inside a signed account
JWT. The answer to "what may this identity publish?" would then live in an opaque blob on the host;
with nkeys it lives in `deploy/nats/ratatoskr.conf`, in this repository, where changing it is a diff
somebody reviews.

The permission set was **verified against a real `nats:2` server** with the same `async-nats` version
the binaries link, rather than reasoned about: both streams were created, a command was published and
acknowledged, the durable consumer was created and fetched from, and publishing `evt.>`, deleting a
stream and reaching the JetStream API from a `cmd.>`-only identity were all refused.

That verification produced one finding worth stating here, because it changes how a failure is
diagnosed: **a client cannot tell a permission denial from an unreachable broker.** A denied publish
is simply never acknowledged, so the outbox records "the message was not acknowledged by the bus",
backs the row off and eventually dead-letters it — the same text it would produce if NATS were down.
The server logs `Publish Violation` with the subject and the nkey, and that log is the only place the
difference is visible.

**What this gives up.** S18 imagined per-role NATS subject allowlists for ingest and scheduler. They
are gone, and they could not have worked: those roles publish through a shared outbox table that has
no notion of which role wrote a row, so a NATS-side allowlist would have constrained the pump rather
than the writer. What constrains them instead is the pair of controls that CAN see the writer — the
per-role PostgreSQL grants of `deploy/postgres/02-grants.sql`, and the `outbox_subject_is_a_valid_subject`
CHECK. Both are weaker than a per-role subject allowlist would have been, and this is the trade.

### 4. Three database roles, two files, in that order

`ratatoskr_edge` owns the three schemas and is the only role with `create` on the database, because
it is the only process that applies `schema.sql`. `ratatoskr_ingest` reaches `platform_ingest` and `operations`;
`ratatoskr_scheduler` reaches `operations` only. **Neither can READ `identity`** — verified, not
asserted — which means the process with the largest unauthenticated attack surface in the system
cannot reach a session credential hash, an OAuth relay or a user's provider identity.

**Amended when the audit writer reached this route.** `ratatoskr_ingest` now holds `usage` on the
`identity` schema and `insert` on exactly one table in it, `audit_events`. `usage` grants the right
to NAME an object and nothing more, and `insert` without `select` is an append-only right, so the
claim above is unchanged and was re-verified table by table: it can append a record and cannot read
back what it wrote, let alone anything else. The narrowing was preferred to the alternatives — a
second audit table in `operations`, or a webhook adapter absent from the audit trail — because a
credential presented at another source's URL is an attributable security decision, and the trail
that omits the most exposed process is the one an incident needs.

There are two SQL files because `schema.sql` creates the schemas: `grant usage on schema identity`
cannot be written before `identity` exists. So: `01-database-and-roles.sql`, then the first edge
start, then `02-grants.sql`.

### 5. The operator listener binds `0.0.0.0`, and the unit is the boundary

The metrics stack is a container on the Docker bridge, and a host loopback port is not reachable from
there. Binding loopback and scraping by name is not available to a host process the way it is to a
container.

So the listener binds `0.0.0.0` and each unit carries `IPAddressDeny=any` with
`IPAddressAllow=localhost 172.16.0.0/12 100.64.0.0/10` — loopback, the bridge, and Tailscale, which
is how `DEPLOYMENT_TARGET.md` says operators reach these surfaces. The LAN is not on that list.

The alternative was a host firewall rule, and it was rejected for the reason `DEPLOYMENT_TARGET.md`
gives about every other host-side fact: a reflash resets it, and a control that a reinstall silently
removes is not a control. A unit file is versioned, reviewed and reinstalled with the deployment.

### 6. Stream and consumer names are code constants, transcribed by the profile

`ratatoskr_commands` / `cmd.>`, `ratatoskr_events` / `evt.>`, and the durable consumer
`platform_edge_projection` now live in `platform_eventing::stream` rather than in a binary, with
their limits — 1 GiB, seven days, refuse-on-full for commands and drop-oldest for events. `deploy/`
names the same strings, and a test compares the two.

An existing stream is still not reconciled — that is the `JetStream` client's behaviour — so
`ensure` reports every differing limit and edge logs it. Fixing one is an operator action against the
broker, which `deploy/nats/README.md` spells out.

## Options considered and rejected

| # | Option | Outcome |
|---|---|---|
| a | Docker Compose instead of systemd | **Rejected.** Per-service Unix users, `ProtectSystem=`, `IPAddressAllow=` and cgroup limits are the isolation this host needs, and a Compose file cannot express them. |
| b | A pump in every role | **Rejected.** Three claimants on one outbox table to avoid a coupling the outbox exists to make safe. |
| c | A `.creds` file per role | **Rejected.** Permissions inside a signed JWT on the host, instead of in a reviewed file in the repository — and two of the three roles need no credential at all. |
| d | A fourth schema for schedules | **Rejected.** Every scheduler transaction would become a cross-schema write. |
| e | Loopback operator listeners plus a host firewall rule | **Rejected.** The metrics stack cannot reach loopback, and a firewall rule does not survive the reflash the profile is being written for. |
| f | A start limit on the units | **Rejected.** No orchestrator here notices a `failed` unit, and every reason a process refuses to start on a first boot is transient. |

## Consequences

- `ratatoskr-scheduler` requires `RATATOSKR__DATABASE__URL` and refuses to start without it, exactly
  as edge and ingest do. There is now no role that serves without a database, so the CI artifact
  smoke test runs `ratatoskr-edge` against a real PostgreSQL and NATS — which also makes it the first
  place `schema.sql` is applied on `aarch64`.
- `journalctl -u ratatoskr-<role>` shows systemd's lines and not the service's. The output is in
  `/mnt/nvme/ratatoskr/logs/`, rotated with `copytruncate`, which is safe only because
  `StandardOutput=append:` opens with `O_APPEND`.
- Enabling a schedule is an operator action against a row, and every schedule is created disabled:
  enabling one starts publishing commands to a domain service that may not be deployed.

## Security and privacy

One host is one trust boundary. The per-service users, the `Protect*=` directives and the address
filter defend against a compromised **process**, not against a compromised host, and that limit is
stated rather than assumed away.

The two credentials that reach disk — the database URL in an `EnvironmentFile` and the nkey seed —
are `0640 root:ratatoskr-<role>`, read by systemd as root and by the process as its own user. Neither
appears in the environment of any other process, in a command line, or in the effective-configuration
log line. A `LoadCredential=` and a configuration loader that reads files would be better and is the
upgrade path; today `platform_core::config` reads the environment only.

## Compatibility and migration

Nothing in `deploy/` has ever been applied to a running system, so there is no migration. The one
change that affects an existing developer setup is `ratatoskr-scheduler` requiring a database, which
`compose.yaml` already provides.

## Validation

`services/edge/tests/deployment_profile.rs` — D-1 to D-6: every unit's stop timeout exceeds the
shutdown ceiling the configuration accepts; every unit starts the binary of its role and validates
its configuration first; every environment template binds its role's operator port and that port is a
scrape target; only the roles that may listen publicly carry a public bind; only edge carries a bus
credential; and the bus profile names the streams the code declares.

The NATS permission set and the PostgreSQL grants were verified by running them, not by review — the
evidence is summarised in decisions 3 and 4.

## What installing it corrected

The profile above was written from documents. Milestone 10 installed it, and four of its statements
were wrong in ways that only a machine could report. They are corrected in `deploy/` and recorded
here because each was a reasonable thing to have believed.

- **PostgreSQL is a container on this host, not a service.** There is no `postgres` user, no
  `postgresql.service`, and no host client of a matching major version. Every administrative command
  enters `shared-postgres`; the units order themselves after `docker.service`; the dump job runs as
  root and reaches the server through `docker exec`. NATS is a container for the same reason, added
  as `deploy/nats/compose.yaml`: one shape for dependencies, another for our own processes, is
  easier to operate than two of each.
- **`Group=` sets the primary group and nothing else.** The units said `Group=ratatoskr`, the shared
  group, while every credential file was `0640 root:ratatoskr-<role>` — so each process was in a
  group that could not read its own database password or nkey seed, and edge failed at startup with
  "the bus credential could not be read". Each unit now names its own group and lists the shared one
  as `SupplementaryGroups=`.
- **The host firewall drops the metrics path.** `ufw` is active with `INPUT policy DROP`, and a
  container reaching a host port crosses `INPUT`. The scrape TIMES OUT rather than being refused,
  which reads like a dead service. Decision 5 above said the unit's `IPAddressAllow=` is the
  boundary, and that is still true — it is the tighter of two gates, not the only one, and
  `deploy/README.md` now installs the wider one. The earlier end-to-end verification of this
  arrangement missed it because it was performed with a Ratatoskr CONTAINER on the monitoring
  network, where the traffic never crosses the firewall; the profile then chose systemd units and
  nobody re-checked. A verification is evidence for the arrangement it was performed on.
- **A least-privilege grant written from the handler misses what the handler calls.**
  `ratatoskr_ingest` had `select, insert, update` on `operations.idempotency_records` and a comment
  saying "delete appears nowhere". `platform_idempotency::reserve` opens by deleting the expired
  reservation for the key. The webhook route answered 504 while the identical path on edge worked,
  because edge owns the schema.

The one code change that came out of it: startup rule V16 checked whether the bus credential file
EXISTS. `is_file()` succeeds on a file the process cannot read, so `check-config` — which each unit
runs as `ExecStartPre`, precisely to answer "will this start?" — reported the configuration valid and
the process then died. It now opens the file.

## Follow-up

- Alert rules on `platform_readiness`, the outbox backlog and scheduler drift, now that Alertmanager
  on that host reaches a person and every series exists.
- `extra_hosts: host.docker.internal:host-gateway` on the metrics stack's own service, which is the
  one change outside this repository that the scrape configuration needs.
- A `ratatoskr.target`, so the deployment can be stopped as one thing.
- ~~An off-host backup destination.~~ **Closed by the encrypted S3-compatible recovery set:** the Pi
  uploads age-encrypted dump, Borg-export and configuration objects, while a separate verifier host
  holds the private identity and performs the weekly restore drill. `/mnt/backup` remains a local
  second volume and is not represented as the off-host copy.
