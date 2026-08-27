# Design: add-edge-gateway-routing

D1. **Routing model** — path-prefix reverse proxy at edge over loopback HTTP, decided in [ADR-0015](../../../docs/adr/0015-edge-routing-model.md) with the rejected alternatives (port/DNS-per-service, per-service credential verification, tunnel-route-per-listener). The ingest webhook adapter keeps its own listener as the recorded exception: source-token traffic, not session traffic.

D2. **Identity forwarding** — edge strips all inbound `x-ratatoskr-*` headers before proxying, then mints `x-ratatoskr-user-id`, `x-ratatoskr-device-id` (device principals only) and `x-correlation-id`. Domain services bind loopback-only and refuse claimless requests, so only edge can originate an authorized call. Services hold no credentials (ADR-0011 keeps the trust model in one place).

D3. **Route classes** — `control`, `stream`, `transfer` are fixed by the ADR; concrete body-size and timeout numbers land as validated configuration rules in this repository's config suite (the V-rule pattern), not as compiled constants, because operators tune them per deployment of one.

D4. **Error normalization** — conforming envelopes pass byte-honest; anything else is rebuilt through `platform_http::fault::render` with stable codes under the `edge.` namespace. The proxy never invents success for an absent upstream.

D5. **OpenAPI representation of proxied surfaces** — deliberately open. ADR-0006 ownership of the generated public document is unchanged; whether proxied routes are aggregated into it or documented per service is decided by the implementing tasks when the first real downstream exists to test against.

D6. **Non-goals** — no service mesh or mTLS between processes on one host, no request transformation beyond header hygiene, no response caching, no buffering of transfer bodies at edge.
