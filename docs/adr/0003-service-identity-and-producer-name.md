# ADR-0003: Service identity and `ProducerName`

## Status

**Accepted for the service-identity half**, 2026-08-18. The NATS subject model, which the ADR backlog
also reserves for ADR-0003, is **deferred to an amendment at milestone 4**, when the first stream,
subject and service credential exist to describe.

## Context

`ProducerName` (`ratatoskr-event-envelope`) is on every message forever, and it is validated against
the closed vocabulary in `contracts.toml [services].known`, which lists exactly one token for this
repository: `ratatoskr-platform`. All four `[[contract]]` entries in that file name the same token as
owner, producer and consumer.

`ARCHITECTURE.md` S18 gives the three binaries separate **runtime** roles with least-privilege
credentials. The question this ADR answers is whether runtime role and wire producer identity are the
same axis.

## Drivers

Irreversibility, in both directions:

- Splitting one identity into three later breaks every consumer that matches on
  `producer == "ratatoskr-platform"` and needs a four-repository changeset.
- Merging three into one later is worse: no consumer can be made to forget the three tokens it has
  already seen on durable messages.

The decision costs one constant today and is unaffordable to reverse at milestone 5.

## Options

| # | Option |
|---|---|
| a | Three per-binary wire identities: `ratatoskr-edge`, `ratatoskr-ingest`, `ratatoskr-scheduler`. |
| b | One identity for the bounded context: `ratatoskr-platform`. |
| c | One identity plus an additive envelope field naming the emitting role. |

## Decision

**(b). Exactly one wire producer identity, `ratatoskr-platform`, shared by all three binaries.
Runtime role and wire producer identity are not the same axis.**

Four citations, each sufficient on its own:

1. `contracts.toml [services].known` lists one token for this repository, and all four
   `[[contract]]` entries name it as owner, producer and consumer. Three identities means a contracts
   change, a re-review of four contract entries, and every consumer allowlist learning three tokens
   instead of one.
2. `crates/event-envelope/src/producer.rs`: "A deployment identity, not an instance identity: never a
   hostname, pod name, region or build version." Its examples are bounded contexts, not processes; a
   per-binary split is a step down that same road.
3. `README.md`: "Internal service topology is never exposed as a client contract."
   `ARCHITECTURE.md` S12: capabilities "describe supported public behavior, not internal topology."
   S15: public error responses "exclude stack traces and internal subject names." A `producer` field
   saying `ratatoskr-scheduler` publishes the control plane's internal decomposition onto a durable
   wire, where consumers can branch on it and it is very hard to take back.
4. S18's three runtime roles are about **deployment** — network exposure, database roles, NATS
   credentials — and are enforced by the credentials the process is given, not by a string inside a
   message.

### Runtime role is carried in telemetry only

`RuntimeRole` is a compile-time constant per binary. It appears in:

- the OpenTelemetry resource attribute `ratatoskr.runtime_role`;
- the `role` label on every metric;
- the `role` field on every span;
- the `role` member of every health and `/version` body.

It never reaches a wire message and it never appears in `contracts.toml`.

### Mechanism at milestone 1

`platform_telemetry::identity::SERVICE_NAME`, a `&'static str` equal to `"ratatoskr-platform"`, is
also the OpenTelemetry `service.name`, so observability identity and wire identity read the same
constant and cannot drift.

`ratatoskr-event-envelope` is a **dev-dependency** at milestone 1. Its only job is test X-1, which
parses `SERVICE_NAME` through `ProducerName` — the same machine-checked guarantee at no runtime
dependency cost, because nothing at milestone 1 constructs a wire value to type. It becomes a real
dependency, and `SERVICE_NAME` gains a `LazyLock<ProducerName>` companion, at milestone 4.

## Consequences

- An event will not say which binary emitted it. The 03:00 question is answered by telemetry instead:
  the publish span carries `ratatoskr.runtime_role` and the failing metric carries `role`.
- If a consumer ever needs the emitting role **as data**, the remedy is option (c) — an additive
  envelope field — and not a second `ProducerName`, because the producer token is also what NATS
  authentication and consumer allowlists are built on. Revisit deliberately at milestone 4 rather
  than discover it.
- The governance vocabulary in `contracts.toml` stays closed at one token for this repository.

## Security / privacy

Positive. The internal decomposition of the control plane stays private, as `README.md` and
`ARCHITECTURE.md` S12 and S15 require. Least privilege is unaffected: it is enforced by per-role NATS
credentials and per-role database roles (S18), neither of which needs a distinct `ProducerName`.

## Compatibility / migration

No change to `contracts.toml` is required, now or at milestone 4 for the identity half. This is the
decision that keeps the closed service vocabulary at one token for this repository.

## Validation

Milestone 1 tests, in `crates/telemetry/tests/identity.rs`:

- **X-1** — `SERVICE_NAME` parses as a `ProducerName` and equals `"ratatoskr-platform"`.
- **X-2** — `RuntimeRole::ALL` has exactly three values, so the `role` label set can never become
  unbounded.
- **X-3** — the OpenTelemetry resource's `service.name` is `SERVICE_NAME`.

## Follow-up

Milestone 4 amends this ADR with the NATS subject model, the per-role service credentials and the
subject allowlists — the other half the ADR backlog reserves for ADR-0003. Until that amendment is
accepted, no stream, subject or service credential is defined anywhere in this repository.
