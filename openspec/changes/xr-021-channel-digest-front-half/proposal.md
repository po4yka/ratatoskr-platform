## Why

The channel-digest chain has no Platform front half: there is no route that lets a user subscribe to a public channel or request a digest run, no route that reads subscriptions and results from the channel-digests service, and the scheduler cannot publish the typed daily occurrence command or accept the typed registration that channel-digests sends.

Cites XR-021 CONTRACTS.md section S08 (Platform command envelopes, public routes, scheduled occurrence, schedule registration command) and the registrar allowlist in section S11 of the same file.

## What Changes

- `PUT /v1/channel-digests/subscriptions/{channel_username}` and `POST /v1/channel-digests/runs`: idempotent session routes that write the operation, the contract `CommandEnvelope` outbox row, the ledger row and the audit record in one transaction.
- `GET /v1/channel-digests/subscriptions`, `/results` and `/results/{result_id}`: typed loopback reads from the channel-digests API using a bounded client with a bearer service secret and the owner header.
- Optional `channel_digests` configuration (`listener` and `service_secret`, set together or absent), with the secret redacted.
- The scheduler publishes a contract `CommandEnvelope` for `channel_digest.schedule.occurrence_requested.v1` schedules and keeps the legacy envelope byte for byte for every other command; the registration handler parses the typed `PlatformScheduleRegistrationRequested` payload.
- `RATATOSKR__SCHEDULING__ALLOWED_REGISTRARS` documents `ratatoskr-channel-digests`, and the operator documentation states which routes are real.

## Capabilities

### New Capabilities

- `channel-digests`: the Platform routes for channel-digest subscriptions, runs and reads.

### Modified Capabilities

- `schedule-registration`: the typed registration payload and the second allowed registrar.

## Impact

- `crates/public-api` (new module, routes, client, tests), `crates/core` (config), `crates/scheduling`, `openapi/openapi.json` (regenerated), `.env.example`, `deploy/systemd/edge.conf.example`, `deploy/README.md`, `DEVELOPMENT.md`, `openspec/specs/schedule-registration/spec.md`.
