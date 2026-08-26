# Tasks: add-edge-gateway-routing

## 1. Decision record

- [x] 1.1 Write `docs/adr/0015-edge-routing-model.md` deciding the routing model, the authentication enforcement point, streaming semantics, route-class budgets, error normalization, failure truthfulness, the loopback trust boundary, and the upload path (documentation — cannot start from a failing test).
- [x] 1.2 Run this repository's full gate on the decision-record commit so the accepted ADR lands on a green tree.

## 2. Gateway implementation

Tasks in this section are seeded from the acceptance criteria and are authored in detail — file paths, test names, assertions — by the session that implements them. None may be ticked until its test has been run and failed for the stated reason first.

- [ ] 2.1 Route table configuration with startup validation rules (unknown service, colliding prefix, missing budget class) — failing config-suite tests before the rule implementations.
- [ ] 2.2 Proxy core: streaming pass-through, hop-by-hop header hygiene, inbound `x-ratatoskr-*` stripping, claim minting — failing unit tests per header rule.
- [ ] 2.3 Authentication enforcement at edge for every proxied prefix: unauthenticated request produces a contract error without calling downstream, asserted via stub downstreams.
- [ ] 2.4 SSE pass-through preserves event order and flush timing on a fixture stream; keep-alive survives the hop.
- [ ] 2.5 Budget enforcement per route class, including incremental rejection of oversized transfer bodies without buffering.
- [ ] 2.6 Failure truthfulness: refused downstream yields truthful 503 with `edge.upstream_unavailable`; slow upstream yields 504; non-conforming downstream bytes are replaced by an envelope from the single construction site.
- [ ] 2.7 Capability aggregation: per-service sections sourced from each service's capabilities document with explicit staleness timestamps.
- [ ] 2.8 Integration test through the real proxy with two stub downstreams covering one happy path and one failure path end to end.
- [ ] 2.9 Deployment profile: loopback port rows in the workspace port table, systemd units where applicable, `deployment_profile.rs` extended to cover the route table.
