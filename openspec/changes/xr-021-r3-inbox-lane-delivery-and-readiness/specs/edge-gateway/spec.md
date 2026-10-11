## ADDED Requirements

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
