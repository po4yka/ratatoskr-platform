# edge-gateway Specification

## Purpose

Authenticated reachability from public clients to domain-service HTTP APIs on the single host: edge terminates authentication once and proxies matched `/v1/<service>` prefixes to loopback listeners, with identity carried as minted claims, bodies streamed under declared budgets, failures surfaced truthfully, and non-conforming downstream responses normalized into contract envelopes. Decided in ADR-0015; implemented by change `add-edge-gateway-routing`.

## Requirements

### Requirement: Authenticated proxying only

The system SHALL proxy a request to a domain service only after edge has authenticated the principal.

#### Scenario: Unauthenticated request never reaches a downstream

- **WHEN** an unauthenticated client calls a proxied `/v1/<service>` prefix
- **THEN** edge answers with a contract error envelope and no downstream call is made

### Requirement: Identity crosses only as minted claims

The system SHALL strip every inbound `x-ratatoskr-*` header and forward only the authenticated user id, optional device id, and minted correlation id.

#### Scenario: Client-supplied identity headers are replaced

- **WHEN** an authenticated request includes forged `x-ratatoskr-*` headers
- **THEN** the downstream receives only Edge-minted claims for that authenticated principal

### Requirement: Streaming pass-through

The proxy SHALL stream request and response bodies without buffering them at edge, preserving SSE event order and flush timing.

#### Scenario: A downstream SSE response remains ordered

- **WHEN** a matched downstream service emits an SSE response
- **THEN** Edge relays its events in order without buffering the complete response body

### Requirement: Budgets enforced per route class

Each route class SHALL carry declared body-size and response-header timeout budgets enforced by Edge.

#### Scenario: A route exceeds its response-header budget

- **WHEN** a downstream response does not produce headers before its route-class timeout
- **THEN** Edge stops waiting and returns the configured timeout failure

### Requirement: Truthful downstream failure

The proxy SHALL return 503 `edge.upstream_unavailable` for a refused listener, 504 for a response-header timeout, and a contract envelope for a non-conforming downstream error.

#### Scenario: A listener refuses a proxied request

- **WHEN** Edge cannot connect to a matched downstream listener
- **THEN** Edge returns `503` with the `edge.upstream_unavailable` contract code

### Requirement: Capability sections carry staleness

Aggregated per-service capability sections SHALL come from each service's own document and carry explicit observation and staleness timestamps.

#### Scenario: A capability section reports its observation time

- **WHEN** Edge returns an aggregated capability section for a domain service
- **THEN** that section includes explicit observation and staleness timestamps from the service document
