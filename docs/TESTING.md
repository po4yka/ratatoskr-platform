# Platform testing strategy

Required suites:

- Session creation/rotation/expiry/revocation and device pairing.
- Authorization matrices and non-disclosure of unauthorized resources.
- Idempotent concurrent request replay.
- Operation state, partial success, cancellation, stale/duplicate events.
- Transactional outbox/inbox, redelivery, and dead-letter behavior.
- OAuth callback and Telegram assertion validation.
- REST/OpenAPI/generated-client compatibility.
- Rate/body/upload limits, audit, health, and readiness.
- `schema.sql` applied to a fresh database, and a second apply that changes nothing.
- Workspace capture flow through one real domain service.

Tests use synthetic identities and local dependencies; no production provider tokens.

## Test-first

A change is planned before it is built, and the plan is a task list in which behaviour arrives in
pairs: one task adds a failing test, the next makes it pass. `openspec/config.yaml` carries that
rule, which is what puts it into every planning and implementation request rather than only into this
document.

The loop:

1. Write the test the scenario names. Run it. Confirm it fails, and read the failure — a test that
   fails because it does not compile has proved nothing about the behaviour.
2. Write the smallest change that makes it pass. Run it again.
3. Refactor only once it is green, adding no test and changing no behaviour.

Two checks stand behind this, and neither of them can see the order:

- `openspec validate --archived`, in `.github/workflows/openspec.yml`, fails when a change was
  archived with a task left unticked.
- A step in `.github/workflows/fleet.yml` fails when this repository holds a manifest and a `ci.yml`
  that never runs a test.

`ratatoskr-workspace/docs/QUALITY_GATES.md` records why the order itself is not checkable, rather
than leaving the gap to be discovered.
