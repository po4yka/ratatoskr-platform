# ADR-0005: NATS subjects, stream classes, and the delivery contract

> Status: Accepted
> Date: 2026-08-18
> Milestone: 4

## Context

[ADR-0003](0003-service-identity-and-producer-name.md) fixed the wire producer identity and
explicitly deferred "the NATS subject half" to this milestone. Nothing else in the repository
constrains subjects. `ARCHITECTURE.md` names message types — `content.capture.requested.v1`,
`platform.operation.progressed.v1` — but never says how a type name becomes a subject, and S15
requires "least-privilege NATS subjects" without saying what the privilege boundary is.

A subject is not a naming convention. It is the unit a NATS credential is granted over, so its shape
decides what a compromised service can publish.

## Drivers

- A credential must be grantable over commands without also granting events. `ARCHITECTURE.md` S18
  gives ingest "limited command publish permissions" and scheduler "allowlisted command publish",
  while edge additionally consumes operation events. A flat namespace cannot express that.
- The subject catalogue and the contract catalogue must not drift into two lists.
- `THREAT_MODEL.md` names event forgery, controlled by "constrained NATS subjects/service
  identities". A constraint that exists only in prose is not a control.
- `ARCHITECTURE.md` S5.3 and S15: an internal subject is never exposed to a client.

## Options

| # | Option | Outcome |
|---|---|---|
| a | The subject IS the contract type name | **Rejected.** `content.capture.requested.v1` and `content.document.extracted.v1` differ only in the aggregate segment, so no wildcard separates a command from an event and every credential must enumerate subjects individually. |
| b | A per-service prefix, `platform.>` | **Rejected.** It publishes internal topology onto a durable wire, which is what ADR-0003 rejected for the producer field, and it makes a consumer's subscription depend on which service happens to emit a fact. |
| c | A class prefix, `cmd.` / `evt.`, then the contract type name | **Chosen.** |
| d | A version-first prefix, `v1.cmd.…` | **Rejected.** The major already terminates the type name, and moving it forward would make `cmd.>` span versions inconsistently with the contract catalogue. |

## Decision

**A subject is `<class>.<contract type name>`**, where class is `cmd` or `evt` and the type name is
the `<context>.<aggregate>.<action>.v<major>` string `ratatoskr-contracts` already governs:

```text
cmd.content.capture.requested.v1
cmd.github.repository.add_requested.v1
evt.platform.operation.progressed.v1
```

The class is the privilege boundary. `cmd.>` and `evt.>` are the two grants S18's role matrix needs,
and a per-context narrowing (`cmd.github.>`) is available without redesigning anything.

Composition rather than invention means there is one catalogue. A subject for which no contract type
exists is unconstructible: `Subject::new` re-validates the contract grammar and refuses anything
outside it.

The grammar is enforced in three places, deliberately:

1. `Subject` in `ratatoskr-platform-eventing`, so a wrong subject cannot be built in Rust.
2. A CHECK constraint on `operations.outbox.subject` and `operations.inbox.subject`, so a row that
   bypassed the Rust layer cannot hold one.
3. The NATS credential itself, at deployment. That one is the actual security control; the first two
   make a violation impossible to reach rather than merely rejected at the edge.

### Delivery

JetStream, not core NATS. `ARCHITECTURE.md` S8.2 assumes at-least-once delivery with redelivery and
a dead-letter path; core NATS is at-most-once with no persistence, so an outbox in front of it would
guarantee nothing.

Every publication carries `Nats-Msg-Id` set to the outbox `message_id`. That engages JetStream's own
duplicate window, so a redelivery of the same outbox row inside the window is collapsed by the
server. It is a first line of defence, not the only one: the consumer's inbox is what makes
deduplication durable beyond the window.

A publication is complete only when the broker acknowledges it. A fire-and-forget publish would let
the outbox mark a row published that the broker never stored, silently converting at-least-once into
at-most-once.

## Consequences

- The subject namespace is `cmd.>` and `evt.>`. A third class would be a new grant everywhere and is
  therefore a decision, not an addition.
- A contract rename is a subject rename. That is correct: they are the same fact.
- `Subject` re-implements the contract type grammar rather than importing a validator, because
  `ratatoskr-contracts` exposes the grammar per type and not as a standalone parser. A test asserts
  the two agree on the forms the architecture document names.

## Security and privacy

The class split is the control S18's role matrix depends on. A subject carries no identifier, no
tenant and no user content — only a type name — so an operator reading broker telemetry learns what
kind of work flows, never whose.

`operations.outbox.last_error` is bounded and newline-free for the same reason `operation_errors`
is: a client library's error chain is multi-line and reaches an operator through that column.

## Compatibility and migration

Pre-release. Nothing is published, so the namespace can still change. After the first deployment a
subject change is a coordinated migration: producers publish to both, consumers subscribe to both,
then the old subject is withdrawn — the same expand/migrate/contract the contracts repository uses.

## Validation

`crates/eventing/src/lib.rs` unit tests cover the grammar in both directions.
`crates/eventing/tests/pump.rs` P-1 publishes to a real `JetStream` stream and asserts the broker
holds exactly one message after two pump passes.

## Follow-up

Stream and consumer naming, the durable consumer configuration, and the per-role NATS credentials
belong to the deployment profile work at milestone 9. This ADR fixes only the subject grammar and the
delivery contract, which are the parts a message shape depends on.
