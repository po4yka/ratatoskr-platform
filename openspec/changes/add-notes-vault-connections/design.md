## Context

ADR-0015 gives every domain service one `/v1/<service>` prefix behind Edge and one route class. Edge
authenticates, strips every inbound `x-ratatoskr-*` header and mints the claims. ADR-0016 pairs a
device and stores its kind. The workspace design of OBS-023 (D4, D5 and D10) asks three new things of
Edge: a claim that names the device kind, a connect call whose body holds Obsidian secrets, and an
operation that only `ratatoskr-notes` reports.

## Decisions

### D1. The device-kind claim is one more minted header

Edge adds `x-ratatoskr-device-kind` beside `x-ratatoskr-device-id` when the principal is a device.
The value is the stored kind (`obsidian_plugin`, `export_agent`, ...), read from
`identity.registered_devices` for the authenticated device. The reserved prefix strip already removes
a client copy. `ratatoskr-contracts` names no constant for this header yet, so Platform holds the
name; the header belongs in the shared claim constants when the contracts change next.

Alternative: put the kind in the session row. Rejected: the kind is a fact of the device, and a
second copy can disagree with it.

### D2. `/blobs` uses the transfer budget inside a control route

The notes route is `control`. Edge registers the sub-path `/v1/notes/connections/{id}/blobs` (and
the paths below it) with the `transfer` body limit, and the proxy selects the `transfer` response
budget for it. One table in the gateway holds the sub-path, so a second one is one row.

### D3. The connect call is an Edge handler that keeps no body

`POST /v1/notes/connections` does not go through the generic proxy. The handler:

1. mints the operation (`notes.connection.connect`, reporter `ratatoskr-notes`) with no idempotency
   key and with an audit record that names only the operation;
2. streams the request body to the notes listener unchanged, with the minted claims and
   `x-ratatoskr-operation-id`;
3. answers `202` with `{ "operation_id", "status": "accepted", "connection": <notes answer> }` when
   notes answers with a success, and relays a notes error envelope as the generic proxy does.

The body is never buffered, parsed or logged by Edge, and the idempotency ledger is not used for this
route: its record would hold a digest of the body, and the notes service already refuses a second
connect of one vault (`notes.connection.vault_already_connected`).

### D4. The report authority row

`notes.connection.connect` lists `ratatoskr-notes` as its only producer, and the connect handler
records that lane on the operation. A report from any other identity is rejected.

### D5. The NOTES identity and the two Knowledge durables

The `NOTES` stanza is the stanza that `ratatoskr-notes` vendors in `deploy/nats/identity.conf`: it
publishes the three note events and the shared operation-report subject, subscribes only to
`_INBOX_notes.>`, and owns no durable. The Knowledge stanza gets the inspect, fetch and acknowledge
grants of `ratatoskr_knowledge_notes_files` and `ratatoskr_knowledge_notes_vaults`.

## Risks / Trade-offs

- [A refused connect leaves an operation `accepted`] → Edge does not mark it failed; the stale
  operation reaper (ADR-0014) harvests it. A retried connect also mints a new operation. The notes
  service must answer `202` early and report the phases on the operation.
- [The connect waits for the notes answer] → the `control` budget (15 s) limits the wait. A `sync`
  login that runs longer answers `504` at Edge, while notes continues; the operation then depends on
  the reports of notes.
- [The report subject is shared] → `NOTES` can forge a report for another producer's operation; the
  authority table and the recorded lane limit what is applied (workspace `fleet-bus-topology`).
