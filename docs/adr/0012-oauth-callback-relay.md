# ADR-0012: How an authorization code reaches the service that owns it

> Status: Accepted
> Date: 2026-08-19
> Milestone: 8

## Context

`docs/ARCHITECTURE.md` S6.4:

> Edge may host public callback routes, but the owning provider service generates or validates
> provider-specific state, exchanges authorization codes, stores encrypted tokens, and records
> granted scopes. Callbacks are relayed using one-time, audience-bound records. Provider tokens never
> enter Platform persistence.

`AGENTS.md` adds: "The Platform may coordinate OAuth callbacks but must transfer credentials only to
the owning provider service through a protected internal flow", and "Never log bearer tokens,
authorization codes, cookies, raw Mini App `initData`, or secret headers."

So the division is settled and the mechanism is not. A provider redirects a browser to a public URL;
the service that can do anything with what arrives is not on the public internet. Something has to
carry an authorization code across that gap, and an authorization code is a bearer credential that
converts into a token.

## Drivers

- The code is a credential in flight. Every additional place it rests is a place it can leak.
- Platform cannot validate the callback. It did not generate the `state`, it holds no client secret,
  and it must not: those belong to the owning service by S6.4. So a callback route is an
  **unauthenticated public endpoint that accepts attacker-chosen values**, and it has to be safe on
  that basis rather than on the basis of the provider being honest.
- The owning service cannot read Platform's database. Cross-service schema reads are forbidden, and
  the domain services do not share a schema with Platform.

## Options

| # | Option | Outcome |
|---|---|---|
| a | Put the code in the command published to the owning service | **Rejected.** The command is written to `operations.outbox.payload`, a `jsonb` column, and then to a `JetStream` file store. That is two durable copies of a live credential, in the two places an operator is most likely to page through while debugging. |
| b | Redirect the browser to the owning service | **Rejected.** It requires every provider service to be publicly reachable, which is the arrangement the facade exists to avoid, and it puts the code in a `Location` header and a browser history. |
| c | A one-time, audience-bound relay record, claimed by the owning service over an authenticated route | **Chosen** — it is what S6.4's own sentence describes. |

## Decision

**A callback creates one row in `identity.oauth_relays` holding the code, and the owning service
claims it exactly once over `POST /v2/oauth/relays/{relay_id}` using a service session whose
principal holds the relay's claim capability. The claim returns the code and destroys it in the same
statement.**

The command published to the owning service carries the relay identifier and **never the code**, so
the outbox, the bus and every log line downstream of them are free of it.

"One-time" is an `update` setting `claimed_at`, so two concurrent claims cannot both succeed: the
second matches no row. "Audience-bound" is `claim_grant` — the record names the capability a caller
must hold, `identity.grants` answers whether they do, and a service holding `oauth.claim.telegram`
cannot take a GitHub relay. Every refusal is the same 404 an unknown relay gets, because which relays
exist and what each needs is not a caller's business.

### Amendment: the binding is a grant, and the first design could never have worked

This ADR first said the relay was bound to the claiming session's **audience**, and the
implementation was written that way. Running it showed the mistake: `identity.sessions.audience`
names the LISTENER a session may be presented at — `edge`, `ingest` — and `session::authenticate`
requires it to equal the listener's own. A session claiming to be `ratatoskr-github` therefore could
not authenticate at the edge listener at all, and every session that COULD authenticate carried the
audience `edge`, which every person also carries. The binding would have been to nothing, on a route
where nothing could ever succeed.

`ARCHITECTURE.md` S7 already makes authorization a capability question, and `identity.grants` — built
at milestone 2 and until now unread — is where capabilities live, with an open vocabulary by design.
So the relay names `oauth.claim.<provider>`, and holding it is what permits the claim. The session
KIND is checked as well: a grant is data an operator writes, and requiring both means a mistake in
one is not enough to leak a code to a person's browser session.

Nothing else in this ADR changes. The correction is recorded rather than edited away because the
wrong version is the one a reader would otherwise reinvent: "bind it to the audience" is the obvious
answer, and the reason it fails is two files apart from where it is written.

### The code is stored, briefly, and that is not the thing S6.4 forbids

S6.4 says provider **tokens** never enter Platform persistence, and they do not: Platform never
exchanges a code and never sees a token. A code is different in kind — it is single-use, expires in
minutes at every provider worth integrating, and is worthless without the client secret Platform does
not hold. Storing it for the seconds between a redirect and a claim is the smallest exposure of the
three options, and it is bounded rather than argued: the row carries a TTL, the claim deletes the
code, and a sweep removes what was never claimed.

It is stored in the clear because it must be returned verbatim; hashing a value that has to be
replayed would be a gesture rather than a control. What makes it safe is that it is short-lived,
single-use and unreachable without a service credential — not that it is obscured.

### The route is unauthenticated, so everything about it is bounded

`state`, `code` and `error` are attacker-chosen. Each is length-bounded and character-bounded at the
edge; `provider` is a closed list, not a string; a relay expires in minutes; and nothing is logged
from the query string. A forged callback therefore costs an attacker one short-lived row addressed to
a service that will reject it for having a `state` it never issued — which is precisely the check
S6.4 assigns to that service and the reason Platform does not attempt it.

## Consequences

- Platform needs no provider client id, client secret, redirect registration or scope list. It needs
  the provider's name and nothing else, which is why `provider` can be a closed list rather than
  configuration.
- The owning service needs a Platform service session. That is an existing session kind
  (`identity.sessions.kind = 'service'`) and needs no new mechanism.
- The browser is redirected to a fixed, configured completion URL rather than one taken from the
  callback: a redirect target read out of an attacker-supplied parameter is an open redirect, and the
  callback is the one route where every parameter is attacker-supplied.
- PKCE, `state` generation and scope checks are the owning service's, as S6.4 assigns them. Platform
  cannot do them and must not appear to.

## Security and privacy

The code exists in exactly two places for its lifetime: one database row and one HTTPS response to
the claiming service. It is not in the command, not in the outbox, not on the bus, not in a log line,
and not in the redirect. An unclaimed relay is swept; a claimed one keeps its row without its code, so
that an operator can still answer "did that callback arrive" without the answer containing a
credential.

## Compatibility and migration

`migrations/0006_oauth_relay.sql` creates the table. No provider service exists yet, so the claiming
half has no consumer today — the route and its contract are what a provider repository implements
against, in the same way ADR-0011 specifies the assertion for `ratatoskr-telegram`.

## Validation

`crates/public-api/tests/oauth.rs`: a callback creates exactly one relay and no command carrying the
code; a claim returns the code once and never twice; a claim by a holder of ANOTHER provider's
capability, by a person's session, and against an unknown or expired relay are all refused
identically; and a test asserts the code appears in no outbox payload.

One implementation constraint worth recording, because it is invisible in the SQL and wrong by
default: `update … returning` reports the NEW row, so `set code = null … returning code` returns
null — the value is destroyed before it can be handed over. The claim is therefore a data-modifying
CTE whose `select` half reads the statement's snapshot, with the `claimed_at is null` guard repeated
inside the `update` because a second concurrent claim blocks on the row lock and then re-evaluates
its own `where` against the new version.

## Follow-up

- The sweep of expired relays joins the retention loop, which is where every other expiry in this
  repository will be collected.
- Rate limiting the unauthenticated callback, with the rest of the public boundary.
