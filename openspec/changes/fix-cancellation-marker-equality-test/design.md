## Context

See [proposal.md](proposal.md). `operations.operations.cancellation_requested_at` is `timestamptz` (schema.sql:622), a PostgreSQL type of microsecond resolution. `crates/operations/src/lib.rs`'s `to_offset`/`from_offset` convert between that and `jiff::Timestamp`, which holds nanoseconds; the conversion loses nothing going into the database beyond what the column can already not hold, and sqlx/PostgreSQL floor the excess on write. The test asserted nanosecond-exact equality between an in-memory `jiff::Timestamp` (the argument passed into the first `request_cancellation` call) and a value read back through that lossy round trip, so the assertion held only when the process clock happened to produce a value already a multiple of 1000ns. On this development machine `jiff::Timestamp::now()` never produces a non-zero sub-microsecond remainder (verified by sampling), which is why the failure never reproduced locally by running the test as-is; forcing a `+604ns` offset into the test's `now()` helper reproduced the exact CI panic (`...544000` vs `...544604`), confirming the mechanism.

## Goals / Non-Goals

**Goals:**

- Make the test assert the real invariant named in its own doc comment — "the repeat must not move the original marker" — using values PostgreSQL actually returned on both sides, so the comparison is exact regardless of storage or in-memory clock resolution.
- Leave the assertion able to fail for the reason it exists to catch: an actual second write to the column.

**Non-Goals:**

- Widen the column, change the stored type, or otherwise touch schema.sql. `timestamptz` losing sub-microsecond precision is normal PostgreSQL behaviour, not a defect.
- Truncate the in-memory expected value to microseconds and keep it as the point of comparison. That would make the test pass because it now guesses the storage granularity correctly, not because it stopped depending on it — the next differing storage boundary (a future column type, a driver change) would reopen the same failure mode. Comparing two already-stored values removes the dependency entirely rather than matching it more carefully.
- Change any production code path. Verified: the only place an in-memory cancellation timestamp is compared against a stored one for equality is this test; every production caller either re-reads the row after commit (`truth()` in `crates/public-api/src/operations.rs`) or uses the in-memory value only to construct a new outbox/audit payload, never to assert equality with a stored column.

## Decisions

Read `cancellation_requested_at` from the database once, right after the first `request_cancellation` call commits (`original_marker`). Compare it, with plain `assert_eq!`, against a second read taken after the repeat's transaction commits, and a third read taken after the refused foreign attempt's transaction rolls back. All three values are `Option<time::OffsetDateTime>` produced by the same column through the same driver, so equality is exact with no arithmetic, no precision assumption, and no dependency on how finely the process clock or the storage type resolves an instant.

## Risks / Trade-offs

- [The rewritten test could pass even if the marker DID move by less than a scheduling jitter that both reads happen to still see as equal] — not applicable here: the two reads are two separate `SELECT`s of the same already-committed row: a moved marker is a different stored value, and `assert_eq!` on the driver-returned type change of a single microsecond value catches any difference the column can hold, which is the same resolution the invariant cares about.
- [Losing the ability to also express "and it equals what the caller believed it wrote"] — the original assertion tried to check that too, but was never able to hold it exactly because of the resolution mismatch; that stronger claim was never validly testable at the field's true precision within one transaction round trip via a fresh clock read, so dropping it loses no coverage that was ever sound.

## Migration Plan

Test-only change to one file; no rollout coordination, no data migration, no consumer impact. Merge once the documented local gate and `openspec validate --all --strict` pass.
