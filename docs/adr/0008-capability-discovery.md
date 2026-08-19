# ADR-0008: What a capability is, and what it is computed from

> Status: Accepted
> Date: 2026-08-19
> Milestone: 7

## Context

`docs/ARCHITECTURE.md` S12 fixes the response shape and one sentence of semantics:

> Capabilities reflect enabled, healthy, and authorized features. They do not reveal internal
> service topology or secrets.

S19 invariant 12 adds: "Capabilities describe supported public behavior, not internal topology."
`AGENTS.md` rule 6: "Capabilities replace frontend assumptions."

The shape is given. What is not given is where each of the three inputs comes from, what the closed
set of capability names is, and — the question that decides whether the endpoint is load-bearing or
decorative — what happens when the document and the routes disagree.

The number is 0008 rather than 0005: `docs/adr/README.md`'s backlog reserved 0005 for this ADR at
milestone 1, but milestone 4 spent that number on the NATS subject model. The backlog is corrected
in this pull request; the file numbers are the truth.

## Drivers

- A capability document that claims a feature the routes do not serve is worse than no document.
  A client that trusts it shows a button that 404s.
- Equally, a document that omits a feature the routes DO serve makes the client hide something that
  works. Both directions are drift, and both must be structurally impossible rather than reviewed.
- S15 keeps operational detail off the public surface. Health is operational detail.
- A public contract can be loosened later and cannot be tightened later.

## Options

| # | Option | Outcome |
|---|---|---|
| a | A free-form list an operator configures | **Rejected.** An operator can then advertise anything, including a name no route serves. The unbounded value also lands in a public response body, which is the shape `ARCHITECTURE.md` S14 bounds everywhere else. |
| b | A closed Rust vocabulary, each entry naming the route family that implements it, evaluated per request against health and grants | **Chosen.** |
| c | Capabilities derived from the route table alone | **Rejected.** It cannot express "the route exists but the dependency it needs is not deployed", which is the one thing a client cannot discover for itself. |

## Decision

**A capability is a variant of a closed Rust enum, `platform_core::Capability`. It is reported to a
caller when all three of its inputs say yes, and each input has exactly one source.**

| Input (S12 word) | Source | At milestone 7 |
|---|---|---|
| **enabled** | Deployment composition: the components the capability needs are configured | `content.submit` needs a bus, because without one its command is written and never published |
| **healthy** | The last probe of those components, the one `/health/ready` already publishes | The database prober's most recent answer |
| **authorized** | `identity.grants` for the calling principal | No capability is grant-gated yet; the filter arrives with the first one that is |

### The vocabulary is what this build serves, not what the product plans

`ARCHITECTURE.md` S12 prints six example names (`content.submit`, `github.catalog`,
`vault.snapshots`, `social.x`, `archive.chatgpt`, `telegram.mini_app`) under "may resemble". Five of
them name features Platform serves no route for. They are **not** in the enum.

This is the whole decision. A capability enters the vocabulary in the pull request that adds the
route family implementing it — one enum variant, one requirement, one line in the test that maps
every variant to a served path. Adding it earlier would put a name on the public surface that the
public surface cannot honour, and no amount of documentation makes that safe.

So at milestone 7 the vocabulary is one entry, `content.submit`, and the endpoint's answer is short
and true. A longer answer would only be longer.

### The endpoint is authenticated

Every other `/v2` route is. Three reasons this one stays with them:

1. The "authorized" input is per-principal by definition, and an anonymous caller has no principal.
2. The "healthy" input is derived from operational state. `content.submit` disappearing tells the
   reader that this deployment's broker link is not working — a fact `/health/ready` deliberately
   keeps on the operator listener (S15).
3. Direction. An authenticated route can be opened to anonymous callers later without breaking a
   single client; an anonymous one cannot be closed.

`minimum_client_versions` is therefore readable only after authentication, which is a real cost: a
client too old to authenticate cannot learn the floor. That is a separate, genuinely public concern,
and it gets its own unauthenticated route on the day a client needs one — not a weakening of this
one.

### `api_version` and `minimum_client_versions` are build constants

They are properties of what this binary serves, not of where it is deployed. An operator cannot
raise the client floor by editing an environment variable, because the floor is a statement about
the API's behaviour and the operator does not decide that.

## Consequences

- `GET /v2/capabilities` returns exactly S12's three members, and the `capabilities` array is
  sorted, so two consecutive responses from an unchanged deployment are byte-identical.
- A capability that is enabled and healthy for one principal is enabled and healthy for all of them
  today. When the first grant-gated capability lands, the response becomes per-principal and the
  route already authenticates, so nothing about the contract changes.
- The endpoint cannot describe a sibling service's health. It describes Platform's ability to
  *accept* the request and get the command onto the bus, which is the only half Platform can
  truthfully speak for. Whether `ratatoskr-extractor` is running is reported by the operation, not
  by this document.

## Security and privacy

The response contains no identifier, no address, no service name and no secret: a capability name is
a feature name, and the vocabulary is closed at compile time, so the body is drawn from a fixed set
of string literals. The one inference an authenticated caller can draw — that a dependency is
unavailable — is why the route is not anonymous.

## Compatibility and migration

Adding a variant adds an array element, which every client must already tolerate, because the point
of the endpoint is that the array changes. Removing one is a breaking change for any client that
gates on it and is therefore a `/v3` concern.

## Validation

`crates/public-api/tests/capabilities.rs` covers: the document's shape against S12; the array sorted
and stable; `content.submit` absent with no bus configured; absent when the last database probe
failed; and present when both hold. A separate test asserts that every variant of `Capability`
resolves to a path the router actually serves, which is the drift gate in the direction that
matters.

## Follow-up

- The grant filter, with the first grant-gated capability.
- An unauthenticated version floor, if a client needs one before it can authenticate.
