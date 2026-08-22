# Design

## The counter lives inside `admit`, not at the call sites

`ActorLimiter::admit` already returns the decision; both production call sites (edge's `Principal`
extractor, ingest's webhook authenticator) and the unit tests go through it. Incrementing there
means a third call site cannot forget the counter, which is the same argument that put the admission
check itself in one place. The label value is decided by the same `bool` the callers already branch
on — no second source for the fact.

## Label set

`outcome` is `admitted` | `refused`, written as `&'static str` constants beside the metric name, the
same device the scheduler's `outcome` label uses. No actor identifier, route, or status becomes a
label: the actor set is unbounded and attacker-influenced, and the per-actor question is answered by
the bucket map's size, not by a cardinality bomb.

## Counting a poisoned limiter

The poisoned-mutex path admits unconditionally and is logged; it counts as `admitted`, because that
is the decision the request received. A decision that was not recorded as made is not invented.

## Testing without a global recorder

The unit test installs a small hand-rolled `metrics::Recorder` inside `metrics::with_local_recorder`
and asserts the counter key and label — no new dependency, no global state, no sleep. The name-set
test T-4 in `crates/telemetry/tests/subscriber.rs` breaks first when `metrics::ALL` gains the name;
that break is the failing test for the registration task.

## Documentation corrections

`README.md` and `AGENTS.md` status sentences are corrected to what the checkout does: rate limiting
exists (bucket, wiring, 429 fault, tests); the backup debt that remains is the off-host copy named
in `deploy/README.md`. These are documentation tasks with no failing test, stated as such in the
task list.
