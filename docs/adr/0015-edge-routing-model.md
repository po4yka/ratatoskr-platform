# ADR-0015: Edge routing to domain services

> Status: Accepted
> Date: 2026-08-26
> Milestone: post-plan (platform parity prompt 5, decision half; the gateway implementation continues under `openspec/changes/add-edge-gateway-routing`)

## Context

The deployment target is one machine, and `cloudflared` is the only public path into it (`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`). Today that path forks twice: edge serves the session-authenticated API on port 8080 and ingest serves provider webhooks on 8181, each a tunnel route of its own. Every further domain service that exposes an HTTP API — knowledge search and user content, the GitHub catalog, vault evidence views, social archives, AI archives — faces the same fork again, and none of them may multiply it: `ARCHITECTURE.md` S2 says public clients communicate only with Edge, S19 invariant 12 says capabilities describe supported public behavior rather than internal topology, and public API principle 1 says clients call Platform, not internal services.

The trigger for deciding now is uploads. Upload-capable clients (mobile plan item 8, export-agent plan item 7) need a shared chunked, resumable, digest-first transfer contract against receiving services' blob receipts, and that contract cannot fix its canonical HTTP binding until the fleet answers a prior question: do upload bytes traverse the edge, or does some other model carry them?

No production outbound HTTP client exists in this repository today; domain services meet Platform only through NATS subjects (ADR-0005), and the only hyper client in the tree holds connections open in a shutdown test. Whatever this ADR chooses introduces the first HTTP hop between platform processes.

## Drivers

- One machine, no second instance, no orchestrator (ADR-0010). Any model that assumes per-service hosts, DNS discovery or replica sets designs hardware that does not exist.
- "A public listener may trust no inbound header it did not itself mint": the tunnel terminates TLS and rewrites headers, so every header arriving from outside is attacker-influenced. Identity forwarded to a domain service must be minted by edge, and inbound copies of anything reserved must be stripped before they can be replayed.
- SSE progress delivery depends on unbuffered streaming; the operation stream already sends a keep-alive every fifteen seconds because proxies close idle connections (`crates/public-api/src/stream.rs`). A proxy that buffers responses breaks the same clients it was meant to serve.
- Failure truthfulness is settled posture: a slow proxied call is classified as a slow upstream (504, not a fabricated 408; F-7 in `crates/http/tests/public_faults.rs`), and operations must never report success nobody observed.
- Errors are stable and actionable: this repository has exactly one `ErrorEnvelope` construction site, `platform_http::fault::render`. Downstream responses that are not contract envelopes cannot be passed to clients as-is without breaking that invariant.
- Body, size and rate limits apply before expensive processing (security requirements). An upload proxy that spools unbounded bodies into memory violates it by construction.
- Ports are allocated in the port table, never chosen ad hoc; new internal listeners join that table like every other bind on the host.

## Decisions

### Path-prefix reverse proxy at edge

Edge gains a config-declared route table — service name, `/v1/<service>` path prefix, loopback listener address, route class — and reverse-proxies matched prefixes to the owning service over loopback HTTP. Clients keep one origin and one base URL; a service becomes reachable by appearing in the route table and advertising itself through capabilities, never by publishing a second public hostname. Unmatched prefixes answer edge's own contract 404; the proxy never guesses a downstream from a client-controlled hint.

### Ingest keeps its listener; the boundary is recorded

The webhook adapter authenticates sources with bearer tokens and accepts raw provider payloads — it is source traffic, not session traffic. Routing it behind edge would add a hop while keeping its distinct authentication, so its tunnel route stays until a session-authenticated ingest surface arrives, at which point that surface lands as an edge-proxied prefix like any other. This is the recorded exception, not a precedent: every future client-facing domain API goes through edge.

### Edge authenticates; services require minted claims

Authentication happens once, at edge, through the existing principal path (`platform_identity::session::authenticate` behind the `Principal` extractor) together with rate limiting and audit. Edge then mints a bounded claim set onto the proxied request — `x-ratatoskr-user-id`, `x-ratatoskr-device-id` when the principal is a device, and the `x-correlation-id` of ADR-0007 — after stripping every inbound header bearing the reserved `x-ratatoskr-*` prefix. Domain services hold no credentials and verify no signatures: they bind their listeners to loopback only and refuse any request that lacks the claim headers, so a claimless request is by construction either a misdirected direct call or a bypass attempt, and both deserve a refusal. Services authorize resource ownership against the forwarded user identity exactly as they would have against a locally validated session.

### Bodies stream through; nothing is buffered

The proxy streams request and response bodies without spooling. SSE pass-through flushes per event and preserves event order and `Last-Event-ID` semantics; chunked upload bodies stream through under their budget class so a receiving service can digest bytes as they arrive. Request transformation beyond header hygiene is out of scope.

### Budgets are route classes declared in the table

Three classes cover the fleet's shapes: `control` for JSON APIs (small body cap, tight total timeout), `stream` for SSE (long idle-read budget, no body), and `transfer` for blob upload/download (large body cap enforced incrementally while streaming, idle-chunk timeout, generous total). Limits are enforced before proxying begins where the shape allows it — a body over the cap is refused with a contract fault, not abandoned mid-stream — and the concrete numbers arrive with the implementing change as validated configuration rules, the same way V18/V19 knobs landed. The classes are the decision; the numbers are configuration.

### Non-conforming downstream responses become contract envelopes at edge

A downstream response whose media type or body is not the contract envelope is replaced at edge by an envelope from the single construction site, with stable codes namespaced `edge.` (for example `edge.upstream_unavailable`, `edge.upstream_invalid_response`). Conforming envelopes pass through byte-honest. Internal topology, storage paths and raw provider errors stay behind the boundary in both paths.

### Downstream absence is truthful

Connection refused, timeout (mapped to 504 per the standing classification) and degraded health probes surface as truthful failures carrying machine-readable reasons. The proxy never fabricates empty success on behalf of an absent service. Capability aggregation extends ADR-0008: per-service capability sections carry explicit staleness timestamps sourced from each service's own capabilities document, so a stale section is visible rather than silently authoritative.

### Loopback trust replaces mesh security

There is no mTLS and no service mesh between processes on one host. The trust boundary is the whole host: PostgreSQL, NATS and the operator listeners already bind host-only, operators arrive over Tailscale, and `cloudflared` is the only public path. New domain-service API listeners join that posture — loopback binds, no ports exposed beyond the host — and the compensating controls at the application layer are the ones decided above: inbound reserved-header stripping, mandatory-claim refusal downstream, and budgets applied before processing. Defense in depth here is the host perimeter plus claim hygiene, not certificates between processes that share a kernel and a filesystem.

### Uploads traverse edge to per-service receipts

Answering the question the transfer contract waits on: client uploads flow through the edge proxy under the `transfer` class to the receiving service's own blob-receipt endpoints. Edge contributes authentication, the minted identity claims, incremental body-budget enforcement and correlation context; the receiving service owns the upload session state, resumability tokens, digest verification and storage placement. Edge never buffers a body to inspect it, terminates the trust chain early, or computes content digests — the receiving service verifies what it stores, because it is the component that keeps it.

### Options considered and rejected

| # | Option | Outcome |
|---|---|---|
| a | Port/DNS-per-service public listeners; clients hold per-service base URLs | **Rejected.** Internal topology becomes a client contract (S19 invariant 12), five public paths replace one, and each needs its own authentication enforcement, rate limiting and capability story — the exact sprawl public API principle 1 exists to prevent. |
| b | Per-service credential verification: edge forwards tokens, each service re-authenticates | **Rejected.** Every domain service would hold or reach identity credentials, duplicating the trust model ADR-0011 keeps in one place, and adding five verification paths where the single-host topology makes one sufficient. |
| c | Extend the status quo: one cloudflared tunnel route per service listener | **Rejected.** The router moves into tunnel configuration, which is invisible to this repository's tests, unversioned and deployment-critical; failure truthfulness, error normalization and budget classes would have no enforcing code to test. |
| d | Path-prefix reverse proxy at edge, minted identity claims, streaming pass-through, route-class budgets | **Chosen.** |

## Consequences

- This repository gains its first production outbound HTTP hop. The hyper client dependency moves from test-only to production, and the proxy inherits the same timeout, drain and shutdown discipline as every listener.
- The route table is validated configuration: unknown service names, colliding prefixes and missing budget classes fail startup like any other rule violation, so a typo cannot silently black-hole a service.
- Domain services joining the fleet allocate loopback ports through the workspace port table and ship a capabilities document; edge aggregates those sections with staleness timestamps.
- The blob-transfer contract crate can now fix its canonical HTTP binding against this model — sessions, chunks and finalization ride the `transfer` class through edge to per-service receipts — which unblocks the ratatoskr-contracts change waiting on this ADR.
- Public base URLs do not change for existing clients; new prefixes appear alongside new capability names in the pull requests that add the routes behind them, per ADR-0008.
- How the generated public OpenAPI document (ADR-0006) represents proxied surfaces is an open question owned by the implementing change, not silently decided here.

## Security and privacy

Identity crosses the hop only as the minted minimum: user id, device id when applicable, correlation id. No credentials, tokens, cookies or provider secrets are forwarded, and stripping inbound reserved headers closes the replay path the tunnel would otherwise leave open. Services refusing claimless requests means only edge can originate an authorized call, and the audit trail records proxied calls at edge where the principal is actually known. Error normalization keeps topology, storage paths and provider diagnostics out of client-visible envelopes in both the conforming and replaced paths. Uploaded bytes are untrusted references end to end: edge bounds them, receiving services quarantine or validate them per their own contracts, and neither treats transport arrival as content approval.

## Compatibility and migration

Additive by construction: existing edge routes, the ingest listener and every current client continue unchanged. A domain service joins by adding a route-table row, a port-table row, a loopback listener and a capabilities document — all server-side; clients discover it through capabilities, needing no release of their own to remain correct. Rollout order across repositories rides the workspace changeset for each service as it lands. No parallel major version, negotiation window or deprecation period exists to plan, because development status allows none.

## Validation

The decision record is validated by review and by the task pair below; behavior is validated when the implementing change lands, failing-test-first per its task list: route-table validation, per-prefix authentication enforcement asserted with stub downstreams (an unauthenticated request produces a contract error without a downstream call), SSE order and flush preservation on a fixture stream, budget enforcement including incremental rejection of oversized transfer bodies, truthful 503/504 on refused and slow downstreams, envelope replacement for non-conforming downstream bytes, capability staleness marking, and a two-stub-downstream integration test through the real proxy.
