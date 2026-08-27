## Context

See [proposal.md](proposal.md). The existing job produces a verified custom PostgreSQL dump on
`/mnt/nvme/backups/ratatoskr` at 02:30 and the host's Borg job copies NVMe to the second local
volume at 03:00. The second volume is not off-host. The Pi is the only Ratatoskr runtime host, so a
verification host must not run any Ratatoskr service.

## Goals / Non-Goals

**Goals:**

- Produce a self-contained, encrypted remote recovery set after the two existing local stages.
- Keep the recovery decrypt key off the Pi and verify the remote database restore weekly on a
  separate verifier host.
- Make every environment-bound script fail closed, support an explicit dry-run, and be covered by
  shell lint plus deterministic tests where possible.

**Non-Goals:**

- Back up Vault, AI, or any other service-owned blob content.
- Replace Borg, change its local retention, or run a second Ratatoskr instance.
- Add continuous WAL shipping, a second cloud, migrations, or a public endpoint.

## Decisions

### Age encryption and AWS CLI v2 upload

The Pi receives only `RATATOSKR_OFFHOST_AGE_RECIPIENT` in a root-owned environment file. It encrypts
each recovery artifact with age before it invokes AWS CLI v2 against the configured S3-compatible
endpoint. The verifier and recovery operator retain the age identity off-board. AWS CLI v2 is chosen
over a custom SigV4 implementation because it is the maintained Debian client for arbitrary
S3-compatible endpoints and exposes endpoint, bucket, region, and scoped credentials through the
standard process environment. `rclone` was considered but requires a generated remote configuration
in addition to credentials, which makes the root-only configuration boundary less direct.

This adds the host packages `age` and `awscli`; they are documented deployment dependencies rather
than Rust dependencies. S3 credentials receive bucket-prefix-only `ListBucket`, `GetObject`, and
`PutObject` permissions; delete and lifecycle-policy permissions are excluded from the Pi. The
operator applies the lifecycle policy out of band with an administrative credential.

### Immutable recovery-set layout

The replication job runs after Borg's existing 03:00 schedule and writes a dated remote prefix with
three age files: a PostgreSQL custom dump, a Borg archive export, and a configuration tarball. The
objects carry a SHA-256 of their ciphertext as metadata and are uploaded only after local creation
and age encryption succeed. The input dump is the completed non-`.partial` file; the Borg export is
made from the selected completed Borg archive; configuration uses an explicit allowlist of Platform,
NATS, systemd, and logrotate files.

The config allowlist deliberately excludes the off-host S3 environment and every age identity. The
recovery operator supplies remote S3 access and the age identity from off-board custody, breaks the
circular bootstrap dependency, and then restores the encrypted configuration payload.

Temporary configuration and Borg exports live only beneath the existing NVMe backup path, have a
trap-based cleanup, and are never written to the boot device. A failed stage returns nonzero and
does not falsely print a completed recovery set.

### Remote lifecycle is enforced by the bucket, not local pruning

The repository ships a parameterized lifecycle-policy generator and an example with ninety-day
expiry for all three prefixes and seven-day abort of incomplete multipart uploads. The bucket policy
is applied by the storage administrator, not the Pi's scoped upload credential. This retains remote
objects independently of the local fourteen-dump policy and avoids an off-host delete operation by a
compromised board. Deterministic tests assert the generated expiration arithmetic and prefix coverage.

### Separate weekly verification host

The confirmed verifier host holds `/etc/ratatoskr/offhost-drill.env` and the age identity with
root-only permissions. Its systemd timer runs the drill weekly against the prior UTC day's object.
The script retrieves that object, decrypts it, creates an ICU-collated scratch database in a local
PostgreSQL 17 container, restores with `--exit-on-error`, checks the three Platform schemas and
constraint counts, drops the database, and prints `PASS` only after cleanup. Each failed stage emits
`FAIL: <stage>` and a nonzero exit.

The verifier is a recovery consumer, not a platform replica: it has no Platform binaries, database
data directory, public listener, or steady-state service. A systemd unit on the Pi cannot perform
this check without retaining the off-board identity, which is why the timer belongs on the verifier.

### Test and dry-run boundary

Shell tests run with generated age identities and fake S3/Borg commands to prove encryption
round-trip, upload selection, config exclusion, and lifecycle-policy arithmetic. A Docker-backed
test starts PostgreSQL 17 and proves the drill restores a real custom dump to scratch. Those tests
are installed and run in CI along with ShellCheck. Actual S3, Borg, service-manager, and credential
operations are environment-bound: their scripts provide `--dry-run`, and the change records the
observed dry-run output using only fixtures.

## Risks / Trade-offs

- [The Borg job moves or exceeds the post-Borg window] → The replication unit validates the selected
  archive and documents the schedule dependency; an absent archive is a visible failed run, not a
  stale upload.
- [Remote object storage is compromised] → Payloads are age-encrypted to an off-board identity and
  the Pi cannot delete retained objects.
- [The verifier's Docker/PostgreSQL is unavailable] → The drill fails with its stage name and its
  systemd unit exposes the failure; it never declares the backup verified.
- [A daily dump cannot meet a lower RPO] → Continuous WAL shipping remains deferred because it
  introduces ongoing credentialed remote streaming, retention, monitoring, and recovery-order work
  beyond this bounded recovery-point objective.
- [The remote provider lifecycle is misconfigured] → The deployment runbook includes the generated
  policy, command, and a post-apply inspection; the Pi does not claim that a local setting applied it.

## Migration Plan

1. Install `age`, AWS CLI v2, and Borg on the Pi; create the dedicated bucket and apply its lifecycle
   policy with an administrator credential.
2. Create the root-only Pi upload environment, install the replication units and scripts, run their
   fixture-backed dry-run, then enable the daily timer after confirming the Borg schedule.
3. Provision the off-host verifier with PostgreSQL 17/Docker, AWS CLI v2, the off-board age identity,
   and its root-only environment; install and run the weekly drill once before enabling its timer.
4. On failure, disable only the new off-host timers and retain the existing dump/Borg jobs. Existing
   local backup behavior is unchanged.
