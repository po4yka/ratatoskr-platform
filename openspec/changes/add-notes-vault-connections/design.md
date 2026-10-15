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

### D2. The device routes use the transfer budget inside a control route

The notes route is `control`. Edge registers the sub-paths `/v1/notes/connections/{id}/blobs` (and
the paths below it), `/changes` and `/manifest` with the `transfer` body limit, and the proxy
selects the `transfer` response budget for them. A change batch of 2 MiB and a manifest of 32 MiB
do not fit in the `control` body budget, and a manifest of 100 000 entries does not arrive and get
reconciled within the `control` response budget on a slow link. One table,
`platform_core::config::GATEWAY_TRANSFER_PATHS`, holds the sub-paths, so a next one is one row.

A row can also name the largest body that the service takes there: 2 MiB for `/changes`, 32 MiB
for `/manifest` (`MAX_BATCH_BYTES` and `MAX_MANIFEST_BYTES` of notes), and 16 MiB for
`/blobs/{token}/chunks/{index}` (`CHUNK_SIZE_MAX_BYTES` of the transfer contract). The proxy refuses
a larger body with `413` before it forwards: by its declared length, or while it streams. Without
the limit a chunk above 16 MiB reaches notes, whose axum `413` is not an error envelope, and Edge
answers `502`. A row below another row is served by the route of the outer row, because the router
cannot hold a parameter and a catch-all at one segment; the proxy still takes the smaller limit.
The limits are copies of the limits of notes, so a change to one of them in notes needs the same
change here. The configuration validation reads the same table: for a service named there,
`public.max_body_bytes` and `public.request_timeout_seconds` must hold the `transfer` budget as well
as the budget of the route class.

The connect path keeps its own rules. A connect body that declares no length and grows above the
`control` budget is refused with `413` while it streams, not reported as an unreachable upstream.
The generic proxy maps the same body-limit error to `413`, so a blob upload without a length that
streams past the `transfer` budget is refused with `413` too.
`POST /v1/notes/connections/` (a trailing slash) is refused at Edge with `404`; without that, it
would reach notes through the generic proxy, which mints no operation and does not keep the body out
of the logs. Other methods at that path go through the generic proxy.

### D3. The connect call is an Edge handler that keeps no body

`POST /v1/notes/connections` does not go through the generic proxy. The handler:

1. mints the operation (`notes.connection.connect`, reporter `ratatoskr-notes`) with no idempotency
   key and with an audit record that names only the operation;
2. streams the request body to the notes listener unchanged, with the minted claims and
   `x-ratatoskr-operation-id`;
3. answers `202` with `{ "operation_id", "status": "accepted", "connection": <notes answer> }` when
   notes answers `202`, and relays a notes error envelope as the generic proxy does;
4. terminates the operation as `failed` before it answers, through the one transition applier, when
   the forward fails: notes is not reachable (`edge.upstream_unavailable`), does not answer in time
   (`edge.upstream_timeout`), answers an error (the code of its envelope, or
   `edge.upstream_invalid_response` when the answer is not an envelope), answers a status other than
   `202` that is not an error, a redirect included, or a `202` that is too large or not JSON
   (`edge.upstream_invalid_response`; Edge does not relay it, because a redirect would send the
   client and its secrets to the place it names), or the request body fails while Edge sends it
   (`platform.request.invalid`, not an unreachable upstream). The error and the progress entry keep
   only the code and the fixed message `The connect did not complete.`, never the notes message. The
   error envelope has no field for the operation id (contracts ADR-0008 keeps `extensions` empty), so
   the failed answer does not name it.

Steps 2 to 4 run in a task of their own, and the handler waits for it. A client that disconnects
drops the handler, and the task still ends the operation, so the client does not wait for the
stale-operation reaper. The request timeout of the public listener does not stop the task, so the
`control` response budget of the notes route bounds it, the read of the answer included; when it
expires, the operation is `failed` with `edge.upstream_timeout`. Two writers of one
operation, for example this task and a report of notes, classify under the row lock of the one
transition applier, so the operation gets one terminal transition and one error.

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

- [A failed connect ends its operation at Edge] → a retried connect mints a new operation. If
  the stale reaper is the only path, a client polls an open operation for a day; Edge therefore
  terminates it at once (D3 step 4). When that write fails, the reaper (ADR-0014) is the fallback.
- [The connect waits for the notes answer] → the `control` budget (15 s) limits the wait. A `sync`
  login that runs longer answers `504` at Edge, and Edge terminates the operation as `failed`, while
  notes can continue. A later report of notes is then a stale or conflicting transition and is not
  applied, so a vault that notes connects after the timeout is not visible on that operation. The
  notes service must answer `202` early and report the phases on the operation.
- [The report subject is shared] → `NOTES` can forge a report for another producer's operation; the
  authority table and the recorded lane limit what is applied (workspace `fleet-bus-topology`).
