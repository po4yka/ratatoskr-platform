# ADR-0002: Operation state machine and progress semantics

## Status

Accepted, 2026-08-18. Binding on milestone 3. Milestone 1 creates no code for this decision; it fixes
the address, the shape, and the rule so that milestone 3 transcribes an accepted decision instead of
inventing one.

## Context

`ratatoskr-contracts` declines to publish a transition table. `crates/operation-contracts/src/status.rs`
states that progress monotonicity "is unenforceable from a single snapshot, and this repository
publishes no transition table because a transition table is a business workflow (`AGENTS.md` hard
boundaries)", and the crate documentation repeats it: "No transition table and no `can_transition_to`."

That is contracts declining ownership, not denying the need. The need is documented on the Platform
side:

- `AGENTS.md` places "operation status, progress, and user-facing error projections" in Platform.
- `ARCHITECTURE.md` S5.4: "Duplicate or out-of-order events cannot regress a terminal status."
- `ARCHITECTURE.md` S19 invariant 10: "Terminal operation states do not regress."
- `DOMAIN.md` invariant 4: "Progress cannot move a terminal operation backward."
- `ARCHITECTURE.md` S17 lists "operation state transitions" as a **unit** test subject. One unit,
  therefore one home.

So Platform owns the rule, and the only open question is where it lives and when it is written.

## Drivers

- Exactly one implementation of a rule that three code paths will need: the operation projection at
  milestone 3, the SSE progress layer at milestone 6, and the stale-operation reconciler at
  milestone 9.
- At-least-once delivery makes duplicates and reordering normal traffic, not error conditions
  (`ARCHITECTURE.md` S19 invariant 7).
- The decision is cheap now and expensive after three disagreeing `match` expressions exist.

## Options

| # | Option | Outcome |
|---|---|---|
| a | Each consumer decides for itself | **Rejected.** Produces an operation whose reported status depends on which code path touched it last. |
| b | Put the table in `platform-core` | **Rejected.** `platform-core` is infrastructure (runtime role, configuration, the internal error type); a transition table is a business workflow. |
| c | Ask `ratatoskr-contracts` to publish it | **Rejected.** Contracts has explicitly declined, and `AGENTS.md` agrees with the boundary it drew. |
| d | A dedicated `crates/operations` | **Chosen.** |
| e | Write it at milestone 1 | **Rejected.** A transition function with no operation record, no schema, no projection and no caller is dead code; this ADR removes the risk that the code would exist to remove. |

## Decision

The table lives at `crates/operations/src/transition.rs`, package `ratatoskr-platform-operations`,
created at **milestone 3**, as a pure function over the contracts enum `OperationStatus`:

```rust
#[must_use]
pub fn apply(current: OperationStatus, incoming: OperationStatus) -> Transition;

#[non_exhaustive]
pub enum Transition {
    /// Legal. The projection must apply `incoming`.
    Advance(OperationStatus),
    /// The identical status re-delivered. A no-op plus a counter — this is what makes
    /// at-least-once redelivery idempotent (`ARCHITECTURE.md` S19.7).
    Duplicate,
    /// An older status after a newer one. Ignored plus a counter, NOT an error: a late `running`
    /// after `succeeded` is normal under at-least-once delivery.
    Stale,
    /// Two different terminal statuses. Rejected, logged at ERROR and alarmed: two producers
    /// disagreeing about the outcome is a real defect that must not be silently absorbed.
    Conflict,
}
```

No `async`, no database, no I/O, no `&self`. Exhaustively testable over all 7x7 status pairs.

### The rule, as a rank function

Expressed as a rank rather than a 7x7 matrix, derived from `DOMAIN.md`'s lifecycle line and invariant
4 and `ARCHITECTURE.md` S5.4:

| Rank | Statuses |
|---|---|
| 0 | `accepted` |
| 1 | `queued` |
| 2 | `running` |
| 3 | `succeeded`, `partially_succeeded`, `failed`, `cancelled` |

- `rank(incoming) > rank(current)` → `Advance`. **Skips are legal**, because at-least-once delivery
  loses and reorders messages: `accepted → running` and `queued → succeeded` must both be accepted.
- Equal rank and the same status → `Duplicate`.
- `rank(incoming) < rank(current)` → `Stale`.
- Equal rank 3 with a different terminal status → `Conflict`.

### The prohibition

No crate other than `ratatoskr-platform-operations` may branch on `OperationStatus` to decide whether
a change is legal. Enforcement is type-level rather than lint-level: the operations crate exposes no
other mutator of the status field, so no other crate *can* write one. Reviewers additionally grep for
`OperationStatus::` outside that crate.

## Consequences

- One home, one test suite, one place to change the rule.
- Milestone 3 transcribes an accepted ADR rather than reinventing it under schema pressure.
- `ratatoskr-operation-contracts` becomes a workspace dependency at milestone 3, pinned to the same
  `ratatoskr-contracts` revision the other contract crates already use.
- `Duplicate` and `Stale` are counted, not logged as failures; only `Conflict` is an alarm.

## Security / privacy

No direct effect. Indirectly, a status that can regress lets a cancelled or failed operation be
reported as succeeded, which is the truthfulness failure `REQUIREMENTS.md` forbids: "Operation state
and partial effects are truthful and replay-safe."

## Compatibility / migration

Nothing to migrate: no operation record exists. Adding a lifecycle status is a `ratatoskr-contracts`
major version, and `OperationStatus` is `#[non_exhaustive]`, so `apply` must keep a catch-all arm that
returns `Conflict` for an unknown pairing rather than guessing a rank.

## Validation

At milestone 3:

- an exhaustive 7x7 table test over `OperationStatus::ALL`;
- an out-of-order redelivery test (`running` after `succeeded` yields `Stale`, not `Advance`);
- a test that no terminal status advances to another status;
- a grep test asserting that no crate outside `ratatoskr-platform-operations` matches on
  `OperationStatus`.

## Follow-up

Milestone 3 creates the crate and the file. This ADR is amended only if the operation lifecycle in
`DOMAIN.md` changes.
