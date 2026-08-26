# ADR-0016: Device credentials and pairing

> Status: Accepted
> Date: 2026-08-26
> Milestone: post-milestone 8

## Context

Milestone 8 established a public authentication route only for a short-lived, signed Telegram
identity assertion (ADR-0011). It did not define how a mobile app, browser extension, or headless
export agent becomes a registered device, retains an authenticated session, or is revoked. The
identity schema already owns registered devices, sessions, rotating refresh tokens, grants, and
audit events, but none of those rows yet make this lifecycle observable to a client.

The four dependent clients need one authority boundary: a user must explicitly approve a named
installation from an already authenticated primary session; the installing client must receive no
provider credential; and a user must be able to see and revoke every device and session. The Edge
process is publicly reachable, so it must not hold a signing private key merely to enroll devices.

## Drivers

- A device needs an independently revocable credential and a short-lived request credential.
- Pairing must require an existing authenticated user decision, not an unauthenticated device claim.
- Compromise of Edge must not create an assertion issuer able to impersonate every user.
- A stolen pairing code must have a small and bounded useful window; repeated guessing must be
  bounded durably across a process restart.
- Revoking a device must atomically prevent every session and refresh credential bound to it from
  authenticating.
- The settings surface needs owner-scoped lists with last-seen information and no credential data.

## Options

| # | Option | Outcome |
|---|---|---|
| a | Give every device a public key and require a signed assertion for every request | **Rejected.** It introduces key generation, proof-of-possession, key rotation, and client crypto interoperability before the clients have a basic pairing flow. It also does not remove the need for an access-session lifecycle. |
| b | Reuse the ADR-0011 public-key assertion format for devices | **Rejected.** Telegram is a separate issuer with a private key Platform must not hold. A device has no trusted issuer before pairing, so Platform would either have to mint assertions itself or trust an unauthenticated key claim. Neither establishes user approval. |
| c | Pair a device using a short-lived, single-use code and issue a random per-device root secret with short-lived rotating sessions | **Chosen.** |

## Decision

### Pairing is an explicit primary-session approval

`POST /v1/devices/pairing-codes` accepts an authenticated **primary** session and may pin an
expected `kind` (`mobile`, `browser_extension`, or `export_agent`) plus a bounded approval label.
A primary session is a live session not established by an already paired device. This prevents a
compromised child device from enrolling further devices. The user interface MUST show the expected
kind when one was pinned, its approval label, expiry, and a warning that redeeming the code
authorizes that installation; creating the code is the user's approval action.

The response returns a high-entropy, opaque pairing code once. It expires after ten minutes, is
single-use, is stored only as a digest, and is bound to the approving session, user, and optional
expected kind. The primary interface transfers it out of band (including an equivalent QR
encoding); the device is never treated as approved merely because it knows its own name.

`POST /v1/devices/pair` is public. It presents the code, a required kind, and a bounded
user-facing display name. A matching live code creates the registered device, a `device` session,
and its first refresh credential in one
transaction, then consumes the code. The response contains the short-lived access credential and
the refresh credential once. A mismatched, expired, consumed, or unknown code returns the same
unauthenticated result so the endpoint is not a code-state oracle.

### Credentials are random bearer secrets with rotation

A device receives a random per-device root secret, a bearer access credential for its bound `device`
session, and a random refresh secret. Edge stores only SHA-256 digests of generated 256-bit values.
The root secret opens a replacement device session at `POST /v1/sessions/device` after expiry or
revoke-all; it never authenticates ordinary requests. The refresh secret is represented by a row in
`identity.refresh_tokens`; presenting it at `POST /v1/sessions/refresh` consumes that row, creates
one successor refresh row, and rotates the access credential for the same session atomically.
Replaying a consumed refresh secret revokes its session and is refused. Every refresh family is tied
to exactly one device session, and deleting that device invalidates both the root secret and every
session family it opened.

The access class is fixed to `read_write` in this release. Per-device scopes are explicitly
deferred; introducing them requires an ADR and a contract change rather than an unreviewed field
that clients might mistake for enforcement.

### Limits and denial recording

Each issued code permits five failed attestation/redemption attempts before it is permanently
denied. Pairing codes still carry at least 256 bits of entropy; the budget is defense in depth and
limits a leaked code's trial window, not a replacement for entropy. The public pair route also has
a fixed, process-wide pre-authentication budget because Edge cannot trust a tunnel-provided client
address. Code issuance uses the existing authenticated per-actor limit.

Every code creation, successful pairing, refresh rotation, session/device revocation, and their
handler-level denial paths writes an `identity.audit_events` record with the request correlation.
The denial record never contains a raw code, refresh secret, access credential, or attestation
payload. Authentication extractor failures remain deliberately indistinguishable and do not create
an unauthenticated audit-write amplification path.

### Revocation and visibility

`GET /v1/sessions` and `GET /v1/devices` return only the caller's active rows and last-seen time.
`DELETE /v1/sessions/{id}` can revoke only the caller's live session. `POST /v1/sessions/revoke-all`
revokes every live session of the caller, including the calling session. `DELETE /v1/devices/{id}`
revokes the owned device and all its sessions in the same transaction; those sessions can no longer
authenticate or refresh, so their associated refresh credentials are unusable without deleting
audit-relevant records.

## Consequences

- Device and session lifecycle routes are additive under `/v1`; no parallel API version exists.
- `schema.sql` gains pairing-code state and changes the current schema in place. No migration is
  added, because development data is intentionally disposable.
- `registered_devices.secret_hash` is the device-root secret digest. It is accepted only by the
  explicit device-session opening route and becomes unusable when the device is deleted.
- A device may be paired only from a non-device session. A user who loses all primary sessions must
  authenticate again through an existing primary authentication route before pairing a replacement.
- The public OpenAPI document describes both credentials as returned-once secrets and never includes
  their values in list resources.

## Security and privacy

Pairing and refresh secrets are never logged, persisted in plaintext, or put in audit records.
All comparisons operate on fixed-length digests and all pair failures share one public response.
Owner checks precede existence disclosure for session and device mutations. The display name is a
user-facing label, not identity evidence or authorization input after pairing.

## Compatibility and migration

There are no existing device clients or retained production rows. The current schema is edited in
place and a fresh database is recreated for this feature. Existing Telegram sessions remain primary
sessions and can therefore authorize their first paired device.

## Validation

Tests cover single use, expiry, the five-attempt budget, attestation mismatch, refresh rotation and
replay, atomic device cascade, complete revoke-all, owner isolation, and audit records for grant and
handler denial paths. The generated OpenAPI drift test covers every route and schema addition.

## Follow-up

- WebAuthn/passkeys are not part of this credential model.
- Per-device scopes beyond `read_write` are deferred.
- Push-notification tokens remain outside the identity model.
