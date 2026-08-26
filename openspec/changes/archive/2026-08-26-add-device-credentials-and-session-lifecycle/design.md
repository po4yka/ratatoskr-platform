## Context

See `proposal.md` for motivation and `specs/device-credentials/spec.md` for the externally visible
contract. Current identity persistence has device, session, rotating refresh-token, and audit tables;
public routing only exchanges Telegram assertions. ADR-0016 fixes the security model before code.

## Goals / Non-Goals

**Goals:**

- Preserve one authoritative identity owner: Edge writes only `identity` through narrow identity
  APIs, and no client or other service reads its tables.
- Make pairing, initial credentials, code consumption, and audit either commit together or roll back.
- Make refresh rotation replay-safe and make every revocation observable immediately through the
  same live-session lookup used by authenticated routes.
- Keep OpenAPI generated from the served route table so route/document drift remains impossible.

**Non-Goals:**

- Public-key device proof, passkeys, trusted platform attestations, and push tokens.
- Per-device capabilities beyond the fixed `read_write` class.
- Cross-repository client implementation; this change supplies the contract that those consumers
  implement through their own changesets.

## Decisions

### Durable pairing-code state rather than an in-memory cache

Add `identity.pairing_codes` with only a code digest, approving session/user IDs, expected device
attestation, issue/expiry/use timestamps, and failed-attempt counter. A restart cannot reset a
brute-force budget or resurrect a consumed code. The initial schema is changed in place, as the
development status requires. `registered_devices.secret_hash` stores the digest of the long-lived
device-root secret; it can open a replacement session but never authenticates ordinary requests.
Rotating refresh rows remain bound to individual device sessions.

### One transaction owns each security boundary

The pair transaction locks and consumes the code while inserting the device/session and first
refresh record. It inserts the allowed audit event before commit. A failed attestation increments
the code budget and writes the denial in the same transaction. Refresh locks the presented token,
marks it consumed, writes its successor, rotates the session token hash, and commits one allowed
audit event. A replay marks its session revoked with its denial event in one transaction. Device
deletion updates the owned device and bound sessions under the same transaction; refresh remains
invalid because it always checks session liveness.

### Owner scope is encoded in persistence queries

List and mutation queries take both user ID and resource ID instead of fetching a row then checking
it at each handler. A zero affected-row result is externally a not-found response for both an
unknown and another user's identifier. This gives tenant isolation the same default path as normal
ownership, not a handler convention.

### Rate limits have an authenticated and a pre-authenticated layer

Pairing-code creation goes through `Principal`, hence the existing per-actor limiter. The public
pair/refresh routes use a fixed process-local request budget before database work because tunnel
headers cannot identify a trusted remote party. Per-code failures are durable and limited to five;
the codes use 256 bits of entropy, so neither budget substitutes for secret strength.

### Tests stay at the public boundary with real SQL

New identity integration tests seed the existing disposable database and assert persistence state;
new API integration tests use the router and credential headers. Each behavior starts RED with a
compiling assertion against absent route/persistence behavior, then gets the smallest GREEN change.
The OpenAPI document test remains the artifact drift oracle after generation.

## Risks / Trade-offs

- [A caller who obtains a code can spend its finite budget before the intended device] → Bind the
  code to attestation, make its TTL short, expose only one uniform refusal, and let the primary
  user mint a replacement after authenticating.
- [A public pairing endpoint can be used to create audit load] → Apply a bounded pre-auth route
  budget before database work; all handler denials remain auditable as required.
- [Refresh-token replay revokes an honest session after a client retry race] → Clients must serialize
  refresh calls; replay detection favors containment of credential theft over continued access.
- [Revoked refresh-token rows remain] → They are intentionally retained for replay detection and
  auditability; retention is a separate, already-established lifecycle concern.

## Migration Plan

1. Deploy the schema definition together with Edge; on this development system a fresh database is
   recreated, not migrated.
2. Deploy all new routes atomically in the single-host stop/start window.
3. Roll back by restoring the previous application and recreating the disposable database. No
   retained device credential has a compatibility promise before the first client release.
