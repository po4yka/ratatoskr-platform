## Why

Round 3 of changeset XR-021 re-verified the Round 2 native smokes and the independent verifiers' remaining gaps against `origin/main` and found six defects inside Platform. Every non-Edge NATS identity may subscribe to the shared `_INBOX.>` namespace, so a compromised service can read the JetStream deliveries of every other service. An operation does not record which service it was dispatched to, so a producer of the same family can report for another lane's capture. A receiver's permanent refusal at archive finalize leaves the operation `accepted` and the staging directory on disk, and an upstream 401 reaches the device as its own credential failure. Edge answers `/health/ready` 503 while an optional receiver is down, and its configuration diagnostics name no failing route. The Telegram assertion key has no working example line.

Cites XR-021 CONTRACTS.md sections R3-01 (per-service inbox prefixes), R3-02 (recorded lane), R3-03 (refused delivery and upstream 401), R3-13 (readiness and route diagnostics), R3-12 (assertion key), R3-11 and R3-19 (operator documentation).

## What Changes

- `deploy/nats/ratatoskr.conf`: each of the twelve non-Edge stanzas subscribes only to `_INBOX_<name>.>`; Edge keeps `_INBOX.>`. `deploy/nats/README.md` carries the prefix table and the rule that every service connects with `custom_inbox_prefix`.
- `operations.operations` gains a nullable `reporter` column. `platform_operations::accept` takes `reporter: Option<&str>`, refuses a reporter outside the kind's `AUTHORITY` list, and every production caller passes its lane. The projection and `authority::authorize` require the report producer to equal the recorded reporter for a listed kind.
- `response_from_upstream` maps an upstream 401 or 407 to `UpstreamInvalidResponse`. `finalize_transfer` ends the delivery on a permanent 4xx refusal: session `failed`, operation `failed`, staging removed, refusal returned unchanged.
- `RuntimeState::is_ready` ignores the four archive receiver checks. Gateway route violations carry the route in `key` and `env_var`.
- `edge.conf.example` and `.env.example` activate `RATATOSKR__IDENTITY__ASSERTION_KEY=CHANGE-ME`; `deploy/README.md` gives the procedure and the PostgreSQL provisioning order; `README.md` and two `schema.sql` comments are corrected.
- Deliberate breaks: a service built before this round times out on JetStream requests under the new ACL (deploy one snapshot together); `accept` has a new parameter; `operations.operations.reporter` is a schema edit in place (recreate the database); an upstream 401 is now 502 `edge.upstream_invalid_response`; `/health/ready` no longer fails on a down archive receiver.
- No migration, no version change, no wire shape change.

## Capabilities

### Modified Capabilities

- `bus-topology`: each non-Edge identity subscribes to its own inbox prefix only.
- `operation-report-projection`: an operation records its reporter and accepts reports only from it.
- `edge-gateway`: upstream 401 is never relayed, a refused archive delivery ends the operation, readiness excludes optional receivers, and route diagnostics name the route.

## Impact

- `deploy/nats/*`, `crates/eventing/tests/*`, `services/edge/tests/deployment_profile.rs`, `schema.sql`, `crates/operations/src/*` and tests, `crates/public-api/src/{intake,archives,gateway}*` and tests, `crates/ingest/src/webhook.rs`, `crates/scheduling/src/lib.rs`, `crates/http/src/lifecycle.rs`, `crates/core/src/config/validate.rs`, `services/edge/src/main.rs`, `docs/ARCHITECTURE.md`, `deploy/README.md`, `README.md`.
- Residual risk: `evt.platform.operation.reported.v1` stays one shared subject; X can still read every tenant's operation-report metadata through `ratatoskr_x_extractor_reports`. Per-producer report subjects stay a contract follow-up.
