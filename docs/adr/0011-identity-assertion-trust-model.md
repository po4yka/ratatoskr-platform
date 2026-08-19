# ADR-0011: What an identity assertion is, and what Platform must hold to believe one

> Status: Accepted
> Date: 2026-08-19
> Milestone: 8

## Context

`docs/ARCHITECTURE.md` S6.3 fixes the shape of the exchange and leaves its mechanics open:

> `ratatoskr-telegram` validates raw Mini App `initData` because it owns the bot token. It returns a
> short-lived signed assertion bound to an internal user and intended Edge audience. Edge exchanges
> that assertion for a short-lived Platform session. Platform never receives the Telegram bot token.

`INTERFACES.md` adds what verifying one means: "Assertions validate issuer, signature, audience,
expiry, nonce, and subject binding." Milestone 2 built the store — `identity.identity_assertions`
with a unique `(issuer, nonce)` index — and left every mechanical question open. Milestone 8 has to
answer four of them before it can write a line: what signs the assertion, what Platform holds to
check it, what the token looks like on the wire, and what "bound to an internal user" means when the
mapping from a Telegram id to an internal user is a table only Platform owns.

## Drivers

- Platform is the process on the public internet. Whatever it holds is what an attacker gets.
- `AGENTS.md`: "No provider secrets cross the public boundary." The bot token never arrives, which
  is settled — but the same reasoning applies to whatever replaces it.
- A verifier that reads its algorithm out of the token it is verifying is the oldest defect in this
  category, and no amount of care in a review prevents it a second time.
- The exchange is one route. Machinery it does not need is machinery that can be wrong.

## Options

| # | Option | Outcome |
|---|---|---|
| a | HMAC with a secret shared between Telegram and Platform | **Rejected.** The verifier can mint. A read-only compromise of the public-facing process yields the ability to forge an assertion for any Telegram user, which is every account. |
| b | A JWT (JWS compact), verified with a public key | **Rejected**, though only on the second half. Public-key verification is right; the JWT envelope is not. Its `alg` header exists to be negotiated, and the only safe use is to ignore it and impose one algorithm — at which point the header is a field whose sole purpose is to be a trap for whoever edits this next. It also brings a library whose failure modes are larger than the problem. |
| c | A two-part compact token, signed Ed25519, with the algorithm fixed by the verifier | **Chosen.** |

## Decision

**An identity assertion is `base64url(payload) "." base64url(signature)`, the payload is JSON, the
signature is Ed25519 over the exact payload bytes as they appear in the token, and the algorithm is a
constant in the verifier that is never read from the token.**

Platform holds **only the Ed25519 public key**, supplied as `RATATOSKR__IDENTITY__ASSERTION_KEY`.
A compromise of Platform therefore yields the ability to *verify* assertions, which is worth nothing,
and not the ability to issue them. The issuer holds the private key and the bot token, and Platform
holds neither.

The signature covers the encoded payload rather than a re-serialization of the parsed one. Two JSON
documents that parse equal can encode differently, so verifying a re-serialization would let a
signature be valid for bytes nobody signed.

### The payload

```json
{
  "issuer": "ratatoskr-telegram",
  "subject": "123456789",
  "audience": "edge",
  "nonce": "…16 to 128 characters…",
  "issued_at": "2026-08-19T09:00:00Z",
  "expires_at": "2026-08-19T09:02:00Z"
}
```

Six members, each one of the checks `INTERFACES.md` requires, and no seventh. In particular there is
no `scope` and no `role`: authorization is Platform's, and an assertion that could carry a grant
would make the issuer able to escalate its own bearer.

### "Bound to an internal user" means bound at redemption

S6.3's phrase is ambiguous and this ADR resolves it. The `subject` is the **provider-side** identity —
a Telegram user id as decimal text — because the mapping to an internal user lives in
`identity.identities`, a table `ratatoskr-telegram` must not read and therefore cannot resolve.
Platform resolves it, creating the internal user on first sight, and writes the resolved id onto the
`identity.identity_assertions` row it redeems. That is why milestone 2 made `user_id` nullable there:
it is not knowable until the assertion is redeemed.

### Replay, expiry and skew

The `(issuer, nonce)` unique index IS the replay defence, and the insert happens in the **same
transaction** as the session it mints. A second presentation of the same assertion therefore fails on
the index rather than on a check somebody has to remember to write, and a crash between the two
cannot leave a session minted with its nonce unrecorded.

Expiry is checked against Platform's clock with **no skew allowance**. The two processes are on one
host with one synchronized clock (`ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md`), so a tolerance
would only widen the window in which a stolen assertion is useful. `issued_at` in the future is
refused for the same reason: it is either a broken clock or a forgery, and neither should mint a
session.

## Consequences

- Rotating the key is an operator action: replace `RATATOSKR__IDENTITY__ASSERTION_KEY` and restart.
  There is deliberately no key set and no `kid`. One issuer with one key needs neither, and a key set
  is a selection mechanism, which is a thing that can select wrongly.
- An assertion that arrives when no key is configured is refused as unauthenticated, and
  `GET /v2/capabilities` reports `telegram.mini_app` as unavailable — so a client can tell the
  difference before it tries, which is the whole point of that endpoint.
- The exchange creates internal users. A deployment that does not want Telegram sign-in leaves the
  key unset, and the route then mints nothing.

## Security and privacy

The Telegram user id is stored in `identity.identities.external_id` and in the assertion record's
`subject`, both of which milestone 2 already bounds. Neither the raw `initData` nor the bot token
ever reaches Platform, so neither can be logged by it. The token itself is never logged: it is a
bearer credential for the two minutes it lives, and `AGENTS.md` already forbids logging raw
`initData` for the same reason.

An expired or malformed assertion, an unknown issuer, a wrong audience, a bad signature and a replayed
nonce all produce the same `401`. The difference between them is a fact about our verification that a
caller cannot be allowed to probe.

## Compatibility and migration

Nothing is deployed and no assertion has been issued. `ratatoskr-telegram` does not exist yet either,
so this ADR is the specification it will implement rather than a description of something already
running: the token format above is the contract between the two repositories, and changing it later
is a coordinated change in both.

## Validation

`crates/identity/tests/assertion.rs` covers each refusal separately — unknown issuer, wrong audience,
expired, not yet valid, tampered payload, tampered signature, wrong key, replayed nonce — and asserts
that a valid assertion mints exactly one session and that presenting it twice mints exactly one.

## Follow-up

- `ratatoskr-telegram` implements the issuing half against this document.
- A second issuer, if one ever exists, is what makes a key set worth its selection logic.
