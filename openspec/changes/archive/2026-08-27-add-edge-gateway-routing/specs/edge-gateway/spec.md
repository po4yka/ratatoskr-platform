# edge-gateway Specification

## Purpose

Authenticated reachability from public clients to domain-service HTTP APIs on the single host: edge terminates authentication once and proxies matched `/v1/<service>` prefixes to loopback listeners, with identity carried as minted claims, bodies streamed under declared budgets, failures surfaced truthfully, and non-conforming downstream responses normalized into contract envelopes. Decided in ADR-0015; implemented by change `add-edge-gateway-routing`.

## ADDED Requirements

### Requirement: Authenticated proxying only

The system SHALL proxy a request to a domain service only after edge has authenticated the principal, and a domain service SHALL refuse any proxied-path request that lacks the minted claim headers.

#### Scenario: Unauthenticated request never reaches a downstream

- **WHEN** an unauthenticated client calls a proxied `/v1/<service>` prefix
- **THEN** edge answers with a contract error envelope and no downstream call is made, asserted with a stub downstream that would fail the test if contacted

#### Scenario: Claimless direct call is refused

- **WHEN** a request arrives at a service's loopback listener without the minted claim headers
- **THEN** the service refuses it with a contract error and processes nothing

### Requirement: Identity crosses only as minted claims

The system SHALL strip every inbound header bearing the reserved `x-ratatoskr-*` prefix before proxying and SHALL forward exactly the user id, device id when the principal is a device, and correlation id it mints itself.

#### Scenario: Forged identity header is discarded

- **WHEN** a client sends `x-ratatoskr-user-id: someone-else` through the tunnel
- **THEN** the downstream receives only the user id edge minted from the validated credential, never the client-supplied value

### Requirement: Streaming pass-through

The proxy SHALL stream request and response bodies without buffering them at edge, preserving SSE event order and flush timing.

#### Scenario: Fixture SSE stream traverses unchanged

- **WHEN** a downstream emits a fixture event stream under a `stream`-class route
- **THEN** the client receives the events in order with per-event flushes and no edge-side buffering delay

### Requirement: Budgets enforced per route class

Each route class SHALL carry declared body-size and timeout budgets, enforced before proxying begins where the request shape allows it.

#### Scenario: Oversized upload is refused early

- **WHEN** a `transfer`-class request declares more bytes than its budget allows
- **THEN** edge refuses it with a contract fault before the body is consumed

### Requirement: Truthful failure of absent or misbehaving downstreams

The proxy SHALL surface a refused downstream as a 503 contract fault carrying a machine-readable reason, a slow downstream as 504, and SHALL replace any non-conforming downstream response with a contract envelope from the single construction site.

#### Scenario: Downstream refuses connection

- **WHEN** the route-table listener for a service accepts no connection
- **THEN** the client receives a truthful 503 with reason code `edge.upstream_unavailable`, never a fabricated empty success

### Requirement: Capability sections carry staleness

Aggregated per-service capability sections SHALL be sourced from each service's own capabilities document and SHALL carry explicit staleness timestamps.

#### Scenario: Stale section is visible

- **WHEN** a service's last observed capabilities document is older than its declared freshness budget
- **THEN** the aggregated response marks that section stale rather than presenting it as authoritative
