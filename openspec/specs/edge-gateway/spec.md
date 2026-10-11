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

#### Scenario: A transfer exceeds its route budget

- **WHEN** a proxied request body exceeds the matched route class's declared limit
- **THEN** Edge rejects it before forwarding excess bytes downstream

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

#### Scenario: A service capability refresh fails

- **WHEN** Edge cannot refresh one service's capability document
- **THEN** that service section remains explicit about its last observation and stale state

### Requirement: An upstream 401 or 407 is never relayed

The gateway SHALL answer `502` with `edge.upstream_invalid_response` when a downstream service answers `401` or `407`, because Edge authenticates before forwarding and sends no `Authorization` to a receiver. The log SHALL carry only the class `upstream_unauthenticated` and the route.

#### Scenario: an upstream 401 becomes 502

- **WHEN** a downstream service answers a valid error envelope with status 401 or 407
- **THEN** the client receives 502 with `edge.upstream_invalid_response`

### Requirement: A permanent receiver refusal at finalize ends the operation

When the archive receiver refuses the forwarded finalize with a `4xx` other than `408` and `429`, the same request SHALL set the transfer session to `failed`, record the operation `failed` with `platform.ai_archive.delivery_failed` at stage `delivery`, remove the staging directory and return the receiver's refusal unchanged. A `5xx`, a timeout or the mapped `502` SHALL leave the session open and the operation `accepted`.

#### Scenario: a 403 ends the operation

- **WHEN** the receiver answers finalize with 403
- **THEN** the client receives that 403, the operation is `failed`, the session is `failed`, the staging directory is gone and a second finalize on the same token is 404

#### Scenario: a 413 ends the operation

- **WHEN** the receiver answers finalize with 413
- **THEN** the operation is `failed` and the session is `failed`

#### Scenario: a 503 leaves the operation open

- **WHEN** the receiver answers finalize with 503
- **THEN** the operation stays `accepted`, the session stays `open`, the staging directory stays, and a later finalize against a healthy receiver stores the completion

#### Scenario: an upstream 401 leaves the operation open

- **WHEN** the receiver answers finalize with 401
- **THEN** the client receives 502, the operation stays `accepted` and the session stays `open`

### Requirement: Readiness excludes the optional archive receivers

`/health/ready` SHALL be `200` when the database, the bus, the archive staging root and startup are healthy and the process is not draining, whatever the state of the four archive receiver checks. Those checks SHALL stay in the response body and in `GET /v1/capabilities`.

#### Scenario: a down receiver does not make Edge unready

- **WHEN** both archive receivers are unavailable and everything else is healthy
- **THEN** readiness is 200, the body lists the failed receiver checks with `dependency_unavailable`, and `platform_readiness` is 1

#### Scenario: an unwritable staging root makes Edge unready

- **WHEN** the archive staging root is not writable
- **THEN** readiness is 503

### Requirement: Route diagnostics name the failing route

A configuration violation for a gateway route SHALL carry the route in `key` and `env_var`, one entry per failed field.

#### Scenario: a failed route is named

- **WHEN** the chatgpt route listens on a port other than the allocated one
- **THEN** one violation has key `gateway.routes.chatgpt.listener` and env var `RATATOSKR__GATEWAY__ROUTES__CHATGPT__LISTENER`
