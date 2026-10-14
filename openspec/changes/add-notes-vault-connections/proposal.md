## Why

The workspace change `add-notes-vault-connections` (OBS-023) adds `ratatoskr-notes`, the service that
connects an Obsidian vault to Ratatoskr. Clients reach it only through Edge. Group 9 of that change
gives Platform its part: the Edge route, the device kind of the Obsidian plugin, the claim that names
the device kind, the connect proxy that keeps no part of the Obsidian secrets, the report authority of
the connect operation, and the `NOTES` bus identity with the two Knowledge durables for note events.

This change cites the workspace store for the shared behaviour: `notes-vault-connections`,
`notes-device-channels` and `fleet-bus-topology` in `ratatoskr-workspace/openspec/changes/
add-notes-vault-connections/specs/`, and `docs/NOTES_AND_SITES.md` sections 4 and 7.

## What Changes

- Edge routes `/v1/notes` to the loopback listener `127.0.0.1:8099` with the class `control`.
  `/v1/notes/connections/{id}/blobs` and the paths below it use the `transfer` budget.
- A new registered device kind, `obsidian_plugin`, in `identity.registered_devices` and in the pairing
  codes.
- Every proxied request of a device carries the claim `x-ratatoskr-device-kind` with the stored kind
  of the device. Edge strips a client copy, as it does for every `x-ratatoskr-*` header.
- `POST /v1/notes/connections` is an Edge handler. It mints an operation of kind
  `notes.connection.connect` with the reporter lane `ratatoskr-notes`, forwards the body unchanged
  with `x-ratatoskr-operation-id`, and answers `202` with the operation id and the answer of the
  notes service. Edge writes no part of the body to its database, its outbox, its idempotency
  records, its audit records or its logs.
- The report authority table gets the row `notes.connection.connect` → `ratatoskr-notes`.
- `deploy/nats/ratatoskr.conf` gets the fourteenth identity, `NOTES`, equal to the stanza that
  `ratatoskr-notes` vendors. `DOMAIN_CONSUMERS` grows from 23 to 25 with
  `ratatoskr_knowledge_notes_files` (`evt.notes.file.changed.v1`) and
  `ratatoskr_knowledge_notes_vaults` (`evt.notes.vault.>`), owned by `KNOWLEDGE`, 120 s ack wait,
  unlimited delivery.

## Capabilities

### Modified Capabilities

- `edge-gateway`: the notes route, the device-kind claim and the connect proxy.
- `device-credentials`: the device kind `obsidian_plugin`.
- `operation-report-projection`: the report authority of `notes.connection.connect`.
- `bus-topology`: the `NOTES` identity and the two Knowledge durables.

## Impact

- `crates/public-api` (gateway, a new `notes` module, the route registration), `crates/identity`
  (device kind), `crates/core` (route table validation), `crates/operations` (authority row),
  `crates/eventing` (durables), `schema.sql`, `deploy/systemd/edge.conf.example`,
  `deploy/nats/ratatoskr.conf` and its guide, and the tests that read them.
- Deployment order: reload NATS with the `NOTES` identity, restart Edge so that it creates the two
  durables, then start `ratatoskr-notes`, then Knowledge.
- `ratatoskr-workspace` must move its harness tables to fourteen identities and 25 service durables
  and list `NOTES` as an implemented Edge upstream (workspace tasks 14.3 and 14.4).
