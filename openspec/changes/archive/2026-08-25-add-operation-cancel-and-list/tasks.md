# Tasks: add-operation-cancel-and-list

Each behaviour is a failing-test task followed by a make-it-pass task. New functions are introduced as stubs whose answer is wrong on purpose, so the first run of every new test fails on its assertion, never on a compile error.

## 1. Storage: cancellation classification

- [x] 1.1 Add failing test `cancellation_requests_classify_against_current_truth` in `crates/operations/tests/lifecycle.rs`: seed operations in all seven statuses through `accept` plus `record_status`, call the new `request_cancellation` as the owner, and assert `Requested` with a written `cancellation_requested_at` for `accepted`, `queued`, `running`; `Terminal` truth without a written marker for `succeeded`, `partially_succeeded`, `failed`, `cancelled`. Introduce `request_cancellation` as a stub that always answers `NotFound` so this run fails on those assertions.
- [x] 1.2 Implement `request_cancellation` per design D2 (ownership-checked locked read, guarded marker write) and verify the test passes.

## 2. Storage: idempotent repeats and foreign owners

- [x] 2.1 Add failing test `repeated_and_foreign_cancellation_requests_write_nothing_new` in `crates/operations/tests/lifecycle.rs`: after a first `Requested` outcome, a second call as the owner answers `AlreadyRequested` leaving the original marker timestamp byte-identical; a call naming another owner's operation answers `NotFound` indistinguishably from a missing identifier and changes nothing observable on the row. (Independent RED was impossible because the 1.2 implementation already carried both behaviours; control proven by mutation — flipping the repeat arm to `Requested` fails this test.)
- [x] 2.2 Close any gap the test exposes and verify it passes alongside section 1.

## 3. Storage: races against completion and the reaper

- [x] 3.1 Add failing test `cancellation_races_resolve_to_one_truthful_outcome` in `crates/operations/tests/lifecycle.rs`: drive concurrent transactions on two pools — cancellation against a `record_status(Succeeded)` report asserts exactly one terminal winner and that the loser classifies from post-race truth; cancellation against `reconcile_one` with the staleness predicate satisfied asserts the reaper still fails the operation with code `platform.operation.stale` even though a request marker was recorded. Assert the database transition trigger never fires for either interleaving. (Both implementations predated the test, so it pinned rather than drove them; control proven by mutation — teaching the reaper to skip requested operations fails it.)
- [x] 3.2 Fix whatever ordering the race test exposes and verify sections 1-3 are green together. (No ordering defect surfaced; stable across repeated runs.)

## 4. Storage: owner-scoped filtered listing

- [x] 4.1 Add failing test `owner_listing_orders_filters_and_pages_a_fixture_set` in `crates/operations/tests/lifecycle.rs`: build a fixture of operations for two owners across several statuses, kinds and acceptance times, and assert on the new `list_operations`: newest-first total order; exact `status` and `kind` filters and their conjunction; limit respected with `next_cursor` present only when more rows exist; walking cursors visits every matching row exactly once even when a new operation is inserted mid-walk; exhaustion reports no next cursor; the other owner's fixture never appears. (Stub returned an empty page; failed on the first order assertion.)
- [x] 4.2 Implement `list_operations` with the keyset cursor per design D6/D7 (encode/decode helpers included) and verify the test passes. (Structured anchor at this layer; opaque-string encoding lands with the route.)

## 5. Public API: cancellation route

- [x] 5.1 Add failing route tests in a new `crates/public-api/tests/operations.rs` using the established harness: `a_session_cancels_its_own_pending_operation_once` (202 with the live snapshot; exactly one outbox row with subject `cmd.platform.operation.cancel_requested.v1` carrying the operation identifier, tenant and correlation context; a replayed call adds no second row), `terminal_operations_answer_with_current_truth` (200 snapshots for terminal sources, zero outbox rows), `another_users_operation_is_not_found` (foreign and nonexistent identifiers both 404 `platform.resource.not_found`), `unauthenticated_cancellation_is_refused` (401), and `a_malformed_operation_identifier_is_a_client_error` (400). (All five failed against the absent route before implementation.)
- [x] 5.2 Implement the handler per design D5/D8 — transaction pairing `request_cancellation` with the command envelope and outbox enqueue, audit record, post-commit truth read for the status code — plus its `RouteDoc` and route-table entry, and verify all section 5 tests pass. (The command and audit carry the OPERATION's correlation, keeping one thread per operation lifetime.)

## 6. Public API: listing route

- [x] 6.1 Add failing route tests in `crates/public-api/tests/operations.rs`: `listing_is_scoped_filtered_and_paginated` (tenant isolation over HTTP; valid `state` and `kind` filters; invalid values refused 400; omitted, in-range, zero and above-maximum `limit`; malformed cursor 400; cursor walk over a fixture; summary rows carry lifecycle fields but omit result references, errors and warnings while the singular endpoint still returns them), and `unauthenticated_listing_is_refused` (401). (Both failed against the absent route before implementation.)
- [x] 6.2 Implement the handler, the `OperationSummary` response type with its schema registration, the `RouteDoc` with query parameters, and the route-table entry; verify all section 6 tests pass.

## 7. OpenAPI document

- [x] 7.1 Regenerate `openapi/openapi.json` with `cargo run --locked -p openapic -- generate` and verify the drift suite stays green with `cargo test -p openapic`. Not test-first: generated artifact whose checks already exist.
- [x] 7.2 Confirm the capabilities vocabulary treatment per ADR-0008 — if the existing operation routes carry no capability name, these carry none; otherwise add the name in the same change. Verify with the capabilities tests unchanged and green. Decision-recording task, not test-first. (Resolution: the closed vocabulary names optional deployment components only; read/SSE carried none, and neither do these.)

## 8. Repository gate

- [x] 8.1 Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, the 850-line file bound, and `cargo deny check`; fix findings.
- [x] 8.2 Run `cargo build --workspace --locked`, the full `cargo test --workspace --locked` against PostgreSQL and JetStream, and `cargo build --workspace --locked --release`; all green.
- [x] 8.3 Run `openspec validate add-operation-cancel-and-list --strict` and keep it green.

## 9. Documentation

- [x] 9.1 Update `DEVELOPMENT.md`'s command-family inventory and presence notes for cancellation and listing, and mention both surfaces in README's operation sections. Documentation task, not test-first.
