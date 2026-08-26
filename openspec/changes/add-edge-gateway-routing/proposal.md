# Proposal: add-edge-gateway-routing

## Why

Domain services (knowledge, github, vault, social archives, AI archives) expose HTTP APIs that web, mobile and extension clients must reach on a single-host deployment without five public ports or per-service token systems. Public API principle 1 requires clients to call Platform only; today there is no path from a client to any domain service's HTTP surface. The decision half is recorded and accepted in [ADR-0015](../../../docs/adr/0015-edge-routing-model.md); this change carries the implementation that makes the decision true. It also unblocks the `ratatoskr-contracts` blob-transfer protocol contract, whose canonical HTTP binding waits on exactly this routing model.

## What Changes

- Edge gains a config-declared route table (service name, `/v1/<service>` prefix, loopback listener, route class) and a streaming reverse proxy with hop-by-hop header hygiene.
- Edge authenticates once, strips inbound reserved headers, mints bounded identity claims (`x-ratatoskr-user-id`, `x-ratatoskr-device-id`, `x-correlation-id`) onto proxied requests.
- Route classes (`control`, `stream`, `transfer`) carry declared body-size and timeout budgets enforced before proxying where the shape allows.
- Non-conforming downstream responses are replaced by contract envelopes from the single construction site; downstream absence and slowness surface as truthful 503/504 with machine-readable reasons.
- Capability aggregation gains per-service sections with staleness timestamps; the deployment profile allocates loopback ports per service.

## Capabilities

### New Capabilities

- `edge-gateway`: authenticated path-prefix proxying of client traffic to domain-service loopback listeners.

### Modified Capabilities

- None. Existing edge routes, the ingest listener and current clients are unchanged.

## Impact

- **Code:** `services/edge` (route table, proxy, aggregation), `crates/config` (route-table rules), `crates/http` (proxy primitives if shared), `deploy/` (loopback ports per joined service).
- **Producers/consumers:** every domain service that joins the fleet adds a listener, a port-table row and a capabilities document; clients discover prefixes through `GET /v1/capabilities` and need no release to remain correct.
- **Cross-repository:** `ratatoskr-contracts` cites ADR-0015 for the blob-transfer HTTP binding; each service activation rides its own workspace changeset.
- **Deployment:** new loopback-only listeners; nothing new is reachable from outside the host.
