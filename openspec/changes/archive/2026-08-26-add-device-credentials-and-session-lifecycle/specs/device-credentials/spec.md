## Purpose

Defines the user-approved, revocable credential lifecycle for Ratatoskr devices and sessions without
exposing any stored credential material through the public API.

## ADDED Requirements

### Requirement: A primary session creates a bounded pairing approval

The API SHALL let a live, non-device session create a pairing code with an optional expected
supported device kind and approval label, and SHALL return its opaque code and expiry once to that
session.

#### Scenario: A primary session creates a code
- **WHEN** an authenticated browser or Telegram session submits an optional supported expected kind and bounded approval label
- **THEN** the API returns a ten-minute pairing code bound to that user and optional kind and records an allowed audit event

#### Scenario: A paired device cannot create another pairing code
- **WHEN** an authenticated device session submits a pairing-code request
- **THEN** the API refuses it and records a denied audit event without creating a code

### Requirement: Pairing redeems an approved code exactly once

The API SHALL exchange a matching live pairing code plus a required supported device kind and
bounded display name for one registered device, one device session, one access credential, and one
refresh credential, and SHALL not expose either credential outside that successful response.

#### Scenario: A matching code enrolls a device once
- **WHEN** a caller presents an unexpired code with its bound attestation
- **THEN** the API creates the device and device session atomically, consumes the code, returns the two credentials once, and records an allowed audit event

#### Scenario: A consumed code cannot enroll another device
- **WHEN** a caller presents a code that has already created a device
- **THEN** the API refuses the request, creates no second device or session, and records a denied audit event

#### Scenario: An expired code cannot enroll a device
- **WHEN** a caller presents a code after its expiry
- **THEN** the API refuses the request, creates no device or session, and records a denied audit event

#### Scenario: A mismatched attestation exhausts the code budget
- **WHEN** callers present the right code with a different attestation five times
- **THEN** the code is permanently refused and no later matching presentation can enroll a device

### Requirement: Device refresh credentials rotate and detect replay

The API SHALL rotate a live device session's refresh credential atomically with its access
credential and SHALL refuse a consumed refresh credential.

#### Scenario: Refresh rotates both credentials
- **WHEN** a caller presents an unconsumed refresh credential for a live device session
- **THEN** the API returns a new access credential and a new refresh credential, and the old refresh credential cannot be used again

#### Scenario: Refresh replay revokes the affected session
- **WHEN** a caller presents a refresh credential that was already consumed
- **THEN** the API refuses the request and the associated session can no longer authenticate or refresh

### Requirement: Owners can view and revoke only their active lifecycle state

The API SHALL return only the caller's active sessions and devices with last-seen times, and SHALL
not disclose or revoke another user's rows.

#### Scenario: A list contains only the caller's active records
- **WHEN** a user requests their sessions or devices
- **THEN** the response excludes revoked and expired rows and every record belongs to that user

#### Scenario: Another user's identifier is not revocable
- **WHEN** a user requests deletion of another user's session or device identifier
- **THEN** the API returns the same not-found result as an unknown identifier, changes nothing, and records a denied audit event

### Requirement: Revocation invalidates all affected credentials atomically

The API SHALL revoke a device and every session bound to it in one transaction, and SHALL revoke
every live session when its owner selects revoke-all.

#### Scenario: Deleting a device revokes all of its sessions
- **WHEN** a user deletes one of their devices with multiple live device sessions
- **THEN** the device and every bound session are revoked together and none of their access or refresh credentials can authenticate afterwards

#### Scenario: Revoke-all includes the calling session
- **WHEN** a user requests revoke-all while authenticated by one of several live sessions
- **THEN** every live session for that user, including the calling one, is revoked and no other user's session changes

### Requirement: Lifecycle decisions are auditable without secrets

The API SHALL record an audit event for every lifecycle grant and handler-level denial, correlated
to the request and without raw pairing codes, access credentials, refresh credentials, or
attestation payloads.

#### Scenario: A lifecycle denial is traceable but not sensitive
- **WHEN** a pairing, refresh, or owner-scoped lifecycle request is refused by its handler
- **THEN** an audit event records the action, outcome, target class, and correlation while storing none of the presented secret values
