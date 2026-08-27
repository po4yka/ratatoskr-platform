# Public Status API Specification

## Purpose

Defines an anonymous, sanitized Platform projection that reports current, stale, unknown, and
unavailable public component health without revealing deployment topology.

## Requirements

### Requirement: Public status is independent of authentication

`GET /v1/status` SHALL return HTTP 200 with the same sanitized shape when a credential is absent,
invalid, or valid while Edge can answer.

#### Scenario: Anonymous visitor reads status
- **WHEN** a request without a credential reads `/v1/status`
- **THEN** Platform returns the status document without creating or requiring a session

#### Scenario: Credential cannot enrich status
- **WHEN** anonymous, invalid-credential, and owner requests read the same observation
- **THEN** their status fields and component facts are identical

### Requirement: Status states preserve uncertainty and degradation

The projection SHALL return the four contracted component groups in deterministic order and SHALL
derive overall state from current cached observations. A stale or unknown fact SHALL NOT be reported
as operational.

#### Scenario: Lost downstream stays visible
- **WHEN** a downstream previously succeeded and its latest bounded refresh fails
- **THEN** the component is degraded, retains its observation time, is stale, and degrades overall status

#### Scenario: Never-observed component remains unknown
- **WHEN** a configured component has never succeeded
- **THEN** it is unknown with no observation time and overall status is not operational

#### Scenario: Required storage loss is unavailable
- **WHEN** the latest readiness fact says storage cannot serve work
- **THEN** storage and overall status are unavailable

### Requirement: Status is sanitized without request-time dependency I/O

The route SHALL project only cached RuntimeState and gateway observations and SHALL NOT reveal
internal service names, addresses, versions, identifiers, raw readiness reasons, raw capability
documents, diagnostics, user data, operation data, or credentials.

#### Scenario: Internal failure detail is excluded
- **WHEN** a cached dependency failure contains an address and diagnostic text
- **THEN** the public component contains only its contracted identifier, state, observation time, and stale flag

#### Scenario: Status read performs no fresh probe
- **WHEN** a client requests status while a dependency is unreachable
- **THEN** the response completes from cached observations without waiting for that dependency

### Requirement: Status cannot be reused as unlabelled fresh data

Every status response SHALL carry `Cache-Control: no-store`.

#### Scenario: Intermediary receives status
- **WHEN** Platform returns a status document
- **THEN** its cache policy prevents storage as a current reusable response
