# ADR-0010: One process per role, and why the locks stay

> Status: Accepted
> Date: 2026-08-19
> Milestone: 7 (recorded ahead of the deployment profile at milestone 9)

## Context

Platform was written for a fleet. The deployment target is one Raspberry Pi 5, supervised by systemd,
with no orchestrator and no second machine — described in
`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md` and summarised in `AGENTS.md`.

That fact is not a decision and needs no ADR. What needs one is its dangerous consequence.

Four sentences in this repository justify a control by appealing to horizontal scale:

- `docs/ARCHITECTURE.md` S18: "Horizontal Edge instances are stateless apart from PostgreSQL and the
  event bus. Scheduler uses leases or advisory locks so only one occurrence is emitted per schedule."
- `AGENTS.md`: "Public request acceptance must remain compatible during rolling deployment."
- `docs/adr/0004-migration-layout-and-query-checking.md`: the migration advisory lock, justified by
  "a rolling deployment of several replicas".
- `DEVELOPMENT.md`: the same, for `Database::migrate`.

The controls those sentences justify are built and shipping: the sqlx migration advisory lock, the
outbox claim lease, the idempotency ledger, the inbox, and `Nats-Msg-Id` deduplication. Delete the
justification without replacing it and every one of them reads as speculative machinery for a scale
that will never arrive — which is exactly the shape a cleanup pass removes.

## Drivers

- A control whose stated reason has become false does not survive review. It has to be re-founded on
  a reason that is still true, or it will be removed by somebody acting reasonably.
- The reasons ARE still true, and they were always the stronger ones. A restart overlapping a drain,
  and a `JetStream` redelivery, both happen with exactly one process.
- The repository already contains the pattern: `crates/eventing/src/outbox.rs` founds its lease on
  process death — "a publisher that crashes mid-batch leaves rows claimed; the lease expiring is what
  returns them to the queue" — and never mentions replicas.

## Options

| # | Option | Outcome |
|---|---|---|
| a | Delete the horizontal claims and leave the controls unexplained | **Rejected.** It is the removal one review away. |
| b | Delete the controls along with the claims | **Rejected.** It reintroduces double-application of an event and double-publication of a scheduled occurrence, neither of which needs a second instance to happen. |
| c | Keep the controls, re-founded on restart and redelivery, and say so once | **Chosen.** |

## Decision

**Platform is deployed as exactly one process per role on one host. The migration advisory lock, the
outbox claim lease, the scheduler lease, the idempotency ledger, the inbox and `Nats-Msg-Id`
deduplication are retained as RESTART- and REDELIVERY-correctness controls, and may not be removed on
the grounds that there is only one instance.**

Each is correct for a reason that has nothing to do with replica count:

| Control | Why it is still needed with one process |
|---|---|
| Migration advisory lock | A restart overlapping the previous process's grace window runs `migrate` while the old process still holds connections |
| Outbox claim lease | A publisher killed mid-batch cannot release its rows; the lease expiring is what returns them |
| Scheduler lease | A restart overlapping a drain can otherwise emit one occurrence twice |
| Idempotency ledger | A client retry is a client behaviour, not a topology |
| Inbox | Delivery is at-least-once by contract; the `JetStream` duplicate window is finite and a consumer restarted after it would apply an event twice |
| `Nats-Msg-Id` | The first line of the same defence, on the server side |

A capacity problem is answered by bounding work — a smaller batch, a longer interval, a tighter pool
— never by adding a process.

## Consequences

- `docs/ARCHITECTURE.md` S18's horizontal sentence is replaced; the other three are re-founded on
  restart overlap rather than replica count.
- One caveat, so this ADR does not overclaim: the migration advisory lock is not code this repository
  could delete. It is sqlx's own behaviour inside `Migrator::run`, which `crates/persistence/src/lib.rs`
  only documents. What this ADR protects there is the *documentation* of why it matters.
- The single-node profile — stream retention, consumer configuration, the NATS credential and the
  database roles — is explicitly NOT decided here. It belongs to milestone 9, as
  [ADR-0005](0005-nats-subjects-and-delivery.md) already reserved.

## Security and privacy

One host means one trust boundary. Per-service Unix users and systemd hardening defend against a
compromised process, not against a compromised host, and that limit is stated in
`DEPLOYMENT_TARGET.md` rather than assumed away.

## Compatibility and migration

Nothing in the code changes. This ADR is written before the deployment profile precisely so that
milestone 9 transcribes a decision instead of making one while wiring units.

## Validation

No test asserts an ADR. What can be checked is the absence of the false claim: a grep for
`horizontal`, `replica` and `rolling deployment` across `docs/`, `AGENTS.md` and `crates/` should
return only sentences this ADR re-founded.

## Follow-up

- The `deploy/` units, at milestone 9.
- The Kubernetes vocabulary in the remaining code comments, swept in the same pull request as the
  profile — the reasoning is what a future agent inherits, and it currently points at an orchestrator
  that will never exist here.
