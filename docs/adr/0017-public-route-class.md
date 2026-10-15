# ADR-0017: The `public` route class and the site route classes

> Status: Accepted
> Date: 2026-10-15
> Changeset: SITE-025, workspace change `add-site-publications` (design D7 and D11; specs
> `site-content-delivery` and `site-route-classes`). Workspace ADR-0011 records why sites exist.

## Context

ADR-0015 made Edge the only public path for API traffic and gave its route table three classes:
`control`, `stream` and `transfer`. Each class sets a body budget and a response budget, and each
needs a session: Edge authenticates the caller, mints the identity claims and forwards the request.

SITE-025 adds `ratatoskr-sites`. It owns the releases of the owner's independent sites and serves
them through the Content Delivery API. A site reads that API with no identity, because its visitors
are anonymous. The same prefix, `/v1/sites/{site}/`, also carries the owner, who manages the site
from the web client, and, in later changes, a site owner session and its sign-in exchange.

So Edge needs two more things: a class that serves anonymous traffic, and a rule that decides who
may call each route under `/v1/sites/{site}/`.

## Drivers

- One host. A burst of public traffic must not reach the sites service or the database before Edge
  limits it.
- A credential must never change what an anonymous route returns. Otherwise a shared cache can keep
  an answer that was made for the owner, and give it to a visitor.
- The sites service owns `Cache-Control` and `ETag` of its content, because only it knows when a
  release changes.
- A new route under `/v1/sites/{site}/` must not ship without a decision about its caller.
- The control plane owns no content (ADR-0015, `blob-references`). Edge stores no release.

## Options

| # | Option | Outcome |
|---|---|---|
| a | A second public listener in the sites service, reached directly through `cloudflared` | **Rejected.** It adds a second public path to the host and a second place that must refuse identities. ADR-0015 rejected per-service public listeners. |
| b | The `control` class with a guest session for each visitor | **Rejected.** A session costs one database row for each visitor and makes each cached answer private to one session. |
| c | A fourth class, `public`, in Edge's route table, and one class for every route under `/v1/sites/{site}/` | **Chosen.** |

## Decision

### The `public` class

A route of the class `public`:

- accepts `GET` and `HEAD` only. Edge refuses every other method with `405` and the contract
  envelope, and refuses a request body with `400`. The sites service gets neither request;
- reads no credential. Edge does not read the `Authorization` header or a cookie, and forwards
  neither. It mints an anonymous claim: `x-ratatoskr-session-kind: anonymous` and the correlation
  id of ADR-0007, and no user id or device id;
- sets no cookie. Edge removes `Set-Cookie` from the answer;
- passes the other answer headers of the sites service through, so the sites service owns
  `Cache-Control` and `ETag`. A `304` passes through;
- has two token buckets, both checked before the proxy. One bucket is for each client, keyed on the
  value of `X-Ratatoskr-Client-IP`. One bucket is the total budget of each site. A request over
  either bucket gets `429` with the contract fault and `Retry-After`. The defaults are 120
  requests for each minute for a client and 1200 for each minute for a site. Each bucket table
  holds at most 10 000 keys, as the per-actor limiter of milestone 8 does.

The sites gateway of SITE-026 sets `X-Ratatoskr-Client-IP`. When Edge trusts that header, and from
which peer, is the rule of SITE-026 (`site-gateway`). Until that rule exists, every request without
the header shares one client bucket, so a missing header cannot get a new bucket for each request.

In this change, the only `public` routes are `GET` and `HEAD /v1/sites/{site}/content/*`, proxied
to the sites listener `127.0.0.1:8100`.

### The site route classes

Edge holds one table of the routes under `/v1/sites/{site}/`. Each row has a method set, a path
below `/v1/sites/{site}`, and a class: `management`, `site-origin`, `sign-in` or `release`. Edge
refuses to start when a row has no class, and the refusal names the route. A request under
`/v1/sites/` that no row holds gets `404` at Edge and does not reach the sites service. A request
whose path a row holds, with a method that the row does not hold, gets `405`.

| Route | Class | Who serves it |
|---|---|---|
| `PUT .../manifest` | management | sites |
| `POST`, `DELETE .../manifest/approval` | management | Edge |
| `GET`, `PUT`, `DELETE .../publications` and `.../publications/{publication}` | management | sites |
| `GET .../releases` | management | sites |
| `POST .../releases` | release | Edge |
| `GET`, `HEAD .../content/*` | site-origin, in the class `public` | sites |

The guard of each class:

- **management**: a primary session (one that a paired device did not open, ADR-0016) whose user
  holds the live `platform.owner` grant. No site capability changes this. Any other caller gets the
  stable unauthenticated or forbidden error, and the request does not reach the sites service.
- **release**: the guard of `management`, or a `site_owner` session of the same site whose
  effective set holds `notes.edit`. This change issues no `site_owner` session, so only the first
  guard can pass. Edge refuses every other caller before it writes a command.
- **site-origin**: the `public` class above. SITE-027 adds site owner sessions to this class.
- **sign-in**: no route has it in this change. SITE-027 adds the sign-in exchange.

The sites service is the second line. It refuses the anonymous claim on every route that is not
under `/v1/sites/{site}/content/`.

## Consequences

- The route table is the place where a new site route gets its caller. A row without a class stops
  Edge at startup, so the decision cannot be forgotten.
- Edge serves the approval routes and the build route itself, because the approval is an
  authorisation decision (`identity.site_approvals`) and only Edge publishes commands.
- An anonymous request costs Edge two bucket checks and one loopback request. It costs no session
  lookup and no database query in Edge.
- A client that cannot set `X-Ratatoskr-Client-IP` shares one bucket with every other such client
  until SITE-026 adds the sites gateway.

## Security and privacy

A credential sent to a `public` route goes nowhere: Edge does not read it, does not forward it, and
does not let it change the answer. The anonymous claim carries no identity, so the sites service
cannot serve owner data to it by mistake, and it refuses that claim outside `content/`. Edge
removes `Set-Cookie` from a `public` answer, so a shared cache never stores a cookie. Every
reserved `x-ratatoskr-` header that a client sends is removed, as ADR-0015 requires.

## Compatibility and migration

Additive. The three classes of ADR-0015 do not change. Existing routes do not change. There is one
API version, so there is no version negotiation and no deprecation window.

## Validation

- `crates/public-api/tests/public_route_class.rs`: `write_method_is_refused_before_proxy`,
  `credential_does_not_change_a_public_response` and `per_client_limit_is_enforced_before_proxy`,
  against a stub sites service that counts requests.
- `crates/public-api/tests/site_route_classes.rs`: `unclassified_site_route_fails_start_up`,
  `site_routes_have_their_listed_classes` and `management_route_needs_live_owner_grant`.

## Follow-up

- SITE-026 (`add-site-gateway-hosting`) states when and from which peer Edge trusts
  `X-Ratatoskr-Client-IP`.
- SITE-027 (`add-site-owner-editing-and-live`) issues `site_owner` sessions, adds the `sign-in`
  routes and the second guard of `release`.
- The two bucket sizes are constants in this change. They become validated configuration when an
  operator needs to change them.
