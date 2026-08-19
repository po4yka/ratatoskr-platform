# ADR-0006: REST versioning, and who owns the public OpenAPI document

> Status: Accepted
> Date: 2026-08-19
> Milestone: 5

## Context

`docs/adr/README.md` reserved this number at milestone 1 and left it unwritten, because a policy for a
surface with no routes is a guess. Milestone 5 adds the first two, so it is due.

Two questions, and the second is a genuine contradiction in the documents rather than an open choice.

`ARCHITECTURE.md` S5.3 requires "versioned `/v2` resource-oriented routes" and names OpenAPI "the
public client contract". `INTERFACES.md` says "public routes use generated OpenAPI contracts".
`README.md` line 168 says APIs are "generated from the public OpenAPI contract" — generated FROM it,
the opposite direction.

Meanwhile `ratatoskr-contracts` claims the artifact too: its `README.md` lists "public OpenAPI
specifications" among what it provides, while its own `docs/INTERFACES.md` lists OpenAPI under
**inputs**, "owned by API producers". Both repositories therefore contain one sentence claiming
ownership and one disclaiming it.

## Drivers

- One artifact, one owner. Two repositories generating the same document is the drift the contracts
  repository exists to prevent.
- The route surface is Rust, and the routes are what a client actually reaches. A document that can
  disagree with them is worse than no document.
- `ARCHITECTURE.md` S15: an internal subject, an internal identifier and internal topology never
  reach a client. A generator that walks the route tree cannot leak what the route tree does not
  contain.

## Options

| # | Option | Outcome |
|---|---|---|
| a | `ratatoskr-contracts` authors the OpenAPI; Platform generates routes from it | **Rejected.** It puts the public surface's shape in a repository that, by its own `AGENTS.md`, must not contain business behaviour — and a route's authentication, idempotency and status semantics are exactly that. It also inverts the direction ADR-0001 chose for every other contract: Rust-first, schema generated. |
| b | Platform generates the OpenAPI from its routes; contracts owns the payload TYPES the routes carry | **Chosen.** |
| c | Hand-maintained OpenAPI in Platform | **Rejected.** `DEVELOPMENT.md` already forbids it: "never hand-maintain duplicate endpoint models". |

## Decision

**Platform owns the public OpenAPI document and generates it from its own routes.
`ratatoskr-contracts` owns the types those routes carry and does not describe the HTTP surface.**

The split is: contracts says what an `OperationSnapshot` is; Platform says that
`GET /v2/operations/{id}` returns one, under which authentication, with which failures. Neither can
state the other's half, so neither can contradict it.

`README.md` line 168 is wrong and is corrected by this ADR: routes are the source, the document is
generated. `contracts/docs/INTERFACES.md` is right that OpenAPI is an input owned by API producers,
and `contracts/README.md`'s claim to "public OpenAPI specifications" is the sentence that must
eventually go; that is a change to a sibling repository and is recorded here as follow-up, not made
silently.

### Versioning

The major version is in the path: `/v2/captures`, `/v2/operations/{id}`. Not a header, and not
content negotiation: a path is visible in a log, in a proxy rule and in a NATS-free curl command, and
`ARCHITECTURE.md` S5.3 already fixed the form.

It starts at **2**, not 1. Ratatoskr Next is the second system to serve this surface; a client that
ever spoke to the retired backend would otherwise find two different `/v1` meanings.

A new major is a new path prefix served alongside the old one, never a change in place. The
idempotency ledger stores the route with its version for the same reason: a key reserved against
`/v2/captures` must not be honoured against `/v3/captures`, because the two accept different bodies.

## Consequences

- The OpenAPI document is a build artifact of this repository, drift-checked like contracts' JSON
  Schema is. It does not exist yet: generating it needs more than two routes to be worth the
  machinery, and it arrives with the capability endpoint at milestone 7. Until then the routes ARE
  the contract, which is why they are tested through the real middleware rather than in isolation.
- A client generator consumes the published document, never this repository's source.
- Adding a route is not a contracts change. Changing the shape of a payload a route carries is.

## Security and privacy

A generated document describes only what the route tree contains, so it cannot publish an internal
subject, a database identifier or the deployment topology — the disclosures S15 forbids. A
hand-written one can, and eventually does.

## Compatibility and migration

Nothing is published. When the document is generated at milestone 7 it must match the routes as they
then are; this ADR fixes the direction so that milestone transcribes a decision rather than making
one under deadline.

## Validation

`crates/public-api/tests/capture.rs` exercises both routes through the real public pipeline, which is
the only contract that exists until the document does.

## Follow-up

- Remove "public OpenAPI specifications" from `ratatoskr-contracts/README.md`, as a contracts change
  with its own review.
- Generate and drift-check the document at milestone 7, alongside the capability endpoint.
