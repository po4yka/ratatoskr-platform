## Why

`repeated_and_foreign_cancellation_requests_write_nothing_new` (`crates/operations/tests/cancellation.rs`) fails intermittently in CI, most recently in `ci` / job `gate`, run 32939209335: `assertion left == right failed: the repeat must not move the original marker`, `left: Some(1787726586532544000)`, `right: Some(1787726586532544604)`. The test builds its expectation with `jiff::Timestamp::now()`, which carries nanosecond precision, and compares that in-memory value against `operations.operations.cancellation_requested_at`, a `timestamptz` column (schema.sql:622) that PostgreSQL and sqlx round to microseconds on write. `1787726586532544000` is exactly `1787726586532544604` with its final three digits — the sub-microsecond remainder — floored to zero. The test does not fail because the marker moved; it fails because the test recomputes what the stored value "should" be from an in-memory clock reading of finer resolution than the column can hold, and the two disagree whenever the clock happens to land on a non-multiple-of-1000 nanosecond.

## What Changes

- Rewrite both assertion sites in `repeated_and_foreign_cancellation_requests_write_nothing_new` (previously at lines 187-191 and 214-218) to compare two values PostgreSQL itself returned — the marker read immediately after the first request commits, against the marker read after the repeat, and again against the marker read after the refused foreign attempt — instead of reconstructing an expected value from the in-memory `jiff::Timestamp` passed into the first call.
- No production code changes. `crates/operations/src/cancel.rs` never compares an in-memory timestamp to a stored one on any path a client can reach: `request_cancellation`'s `Cancellation::Requested` return value carries the in-memory `now` only as a value handed back to its immediate caller for outbox/audit payloads (`crates/public-api/src/operations.rs`), and the HTTP response is always built by `truth()`, which re-reads the row PostgreSQL holds after commit rather than trusting the in-memory value. The precision mismatch exists only in this test's own expectation arithmetic.

## Capabilities

No contract or runtime behaviour changes; `skip_specs: true` is set in the change manifest. This is a test-only correctness fix.

## Impact

- `crates/operations/tests/cancellation.rs` only.
- No wire types, schema, generated artifacts, or production code paths.
