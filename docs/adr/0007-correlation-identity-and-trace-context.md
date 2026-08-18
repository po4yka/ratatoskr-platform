# ADR-0007: Correlation identity and trace context

## Status

Accepted, 2026-08-18. Implemented by milestone 1.

## Context

`ARCHITECTURE.md` S16: "Trace context propagates through commands and events via correlation and
causation IDs." `AGENTS.md` requires every accepted request and published command to carry a
correlation ID.

`ratatoskr-contracts` has already fixed the type:

- `EventEnvelope.correlation_id: EntityRef` — **required**, not optional
  (`crates/event-envelope/src/envelope.rs`).
- `OperationSnapshot.correlation_id: EntityRef` — likewise required
  (`crates/operation-contracts/src/snapshot.rs`).
- `ErrorEnvelope.correlation_id: Option<EntityRef>` (`crates/error-contracts/src/envelope.rs`).

All three are namespaced `<kind>:<local_id>`. The remaining question is what Platform carries
internally between minting the value and putting it into one of those slots.

## Drivers

- A `String` would have to be re-parsed — fallibly — at every publish site at milestone 4 and every
  snapshot site at milestone 3, in code paths where a parse failure has no sensible handling.
- `EntityRef` carries semantics a `String` silently loses (contracts ADR-0007): equality is octet
  equality and the local part is case-**sensitive**, but a UUID local part must be canonical
  lowercase. Two `String`s a human would call equal are therefore not the same `EntityRef`, and the
  `causation_id → EventId` join would miss.
- Correlation must exist on failure paths where no operation is ever created.

## Options

| # | Option |
|---|---|
| a | `String` internally, parse at the contract boundary. |
| b | `Option<EntityRef>`, `None` until an operation exists. |
| c | `EntityRef`, always minted. |

## Decision

**(c).** `correlation_id` is `ratatoskr_identifiers::EntityRef` from the first line of telemetry code.
No `String` appears for a correlation anywhere in Platform, at any layer, at any milestone. Text
appears at exactly two boundaries — the `x-correlation-id` response header and the JSON log field —
both through `Display`.

### Minting

Once per request, at the outermost point of the observation middleware, before any handler code:

```rust
CorrelationId::new_v7().as_entity_ref()   // -> correlation:018f…
```

Contracts ships that constructor for exactly this case: "A correlation identity minted by a producer
for work not bound to an operation."

The value is never re-minted. One request has exactly one correlation for its whole life, including
the failure paths where no operation is ever created — body-limit rejection, request timeout, and a
caught handler panic.

A client-supplied `x-correlation-id` is **never** honoured, under any configuration. `AGENTS.md` and
`ARCHITECTURE.md` S15 both state that internal headers are not trusted from public ingress, and a
caller-chosen correlation would let one caller graft its requests onto another's investigation.

### The milestone-3 rule, fixed now

The operation created for a request carries the request's **already-minted** correlation in
`OperationSnapshot.correlation_id`. It is *not* replaced by `operation:<operation_id>`. Two reasons:

- the correlation must exist before the operation does — validation, idempotency reservation and
  authorization failures all happen first;
- one request may later create more than one operation.

`EntityRef`'s kind vocabulary is open, so `correlation:` is legal in that slot, and this is the only
reading under which `OperationSnapshot.correlation_id`'s own documentation holds: "Every event and
error emitted while serving the operation carries the same value."

### Trace context, separately

The W3C `traceparent` is a second, independent axis and is not a correlation.

- A valid inbound `traceparent` is continued.
- An absent or malformed one starts a new trace and never fails the request.
- A client-chosen trace ID is accepted, because contracts documents `TraceId` as "for log correlation
  only; never a business key and never an authorization input"
  (`crates/error-contracts/src/trace.rs`). It is therefore not an authorization surface.

This is the one place `ARCHITECTURE.md` S15's "internal headers are not trusted from public ingress"
is read narrowly. The reading is recorded here so that it is a decision and not an oversight, and it
is scoped to `traceparent` alone: `x-correlation-id` is still rejected.

`trace_id` is **omitted** from an `ErrorEnvelope` when the span context is invalid, rather than
emitted as thirty-two zeros.

## Consequences

- Milestone 4 puts the same `EntityRef` into `EventEnvelope.correlation_id` with **zero conversion**.
  That is the whole payoff of the decision.
- There is no `X-Request-Id`. The correlation **is** the request ID until an operation exists;
  `AGENTS.md`'s "request ID; correlation ID" pair is satisfied by one value at milestone 1, and that
  deviation is stated in `DEVELOPMENT.md`.
- Spans and error envelopes carry a real W3C trace ID even with no OTLP collector deployed, so
  `trace_id` is useful on a laptop and not only in a cluster.

## Security / privacy

- The correlation is a random UUIDv7 and discloses nothing about the requester, the route or the
  deployment.
- Accepting an inbound trace ID lets a caller choose the `trace_id` that appears in a public
  `ErrorEnvelope`. The residual risk is trace pollution, which a caller can already cause by sending
  requests, and the value is never an authorization input and never a business key.
- The correlation is minted server-side, so it cannot be used to collide with or impersonate another
  caller's request identity.

## Compatibility / migration

**`correlation` is deliberately NOT in `contracts.toml [entity_kinds].known`.** `CorrelationId`'s own
documentation states that a fixture carrying `correlation:` must add the token there first, "which is
the governed path that file describes".

`correlation:018f…` therefore parses as `EntityKind::Other("correlation")` — legal on the wire type,
**not legal in a governed contracts fixture**. Milestone 1 emits no events and publishes no fixtures,
so nothing is blocked today.

## Validation

Milestone 1 tests:

- **T-2** (`crates/telemetry/tests/subscriber.rs`) — the minted value parses back through
  `EntityRef::parse` and matches `EntityRef::PATTERN`.
- **R-5** (`crates/http/tests/redaction.rs`) — the value the JSON log line carries is a parsable
  `EntityRef`, so a refactor to `String` fails a test rather than a code review.
- **F-4** (`crates/http/tests/public_faults.rs`) — every rendered fault carries the correlation in
  both the response body and the `x-correlation-id` header.

## Follow-up

**Blocking milestone 4** (open question Q4): a one-line `ratatoskr-contracts` changeset adding
`"correlation"` to `contracts.toml [entity_kinds].known`. Without it, `cargo contracts check` fails on
the first Platform event fixture. Owner: whoever opens milestone 4.
