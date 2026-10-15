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

The sites gateway of SITE-026 sets `X-Ratatoskr-Client-IP`. Edge reads the header only on a
connection from the gateway's address (`sites.gateway_peer`, default `127.0.0.83`), and only when
it holds exactly one IP address. Every other request, and every other connection, has the peer
address as its client key (SITE-026, spec `site-gateway`; the rule is in the follow-up of
ADR-0015). So a missing or malformed header cannot get a new bucket for each request.

In this change, the only `public` routes are `GET` and `HEAD /v1/sites/{site}/content/*`, proxied
to the sites listener `127.0.0.1:8100`.

### The site route classes

Edge holds one table of `/v1/sites` and the routes under `/v1/sites/{site}/`. Each row has a method
set, a path below `/v1/sites`, and a class: `management`, `site-origin`, `sign-in` or `release`. Edge
refuses to start when a row has no class, and the refusal names the route. A request under
`/v1/sites/` that no row holds gets `404` at Edge and does not reach the sites service. A request
whose path a row holds, with a method that the row does not hold, gets `405`.

| Route | Class | Who serves it |
|---|---|---|
| `GET /v1/sites` | management | sites |
| `GET`, `PUT .../manifest` | management | sites |
| `POST`, `DELETE .../manifest/approval` | management | Edge |
| `GET`, `PUT`, `DELETE .../publications` and `.../publications/{publication}` | management | sites |
| `GET .../releases` | management | sites |
| `POST .../releases` | release | Edge |
| `POST .../deploys`, `POST .../deploys/{commit}/activate` | deploy | sites |
| `POST .../sign-in-codes` | management | Edge |
| `POST .../sessions` | sign-in | Edge |
| `GET .../events`, `GET .../drafts/pages/{*slug}`, `GET`, `POST .../drafts/sources/{source_ref}` and `.../blobs`, `POST .../edits`, `GET .../conflicts/{conflict_id}`, `GET .../library`, `GET .../knowledge/search` | site-origin, with the site owner session | Edge, sites, notes or Knowledge (SITE-027) |
| `GET`, `HEAD .../content/*` | site-origin, in the class `public` | sites |

The guard of each class:

- **management**: a primary session (one that a paired device did not open, ADR-0016) whose user
  holds the live `platform.owner` grant. No site capability changes this. Any other caller gets the
  stable unauthenticated or forbidden error, and the request does not reach the sites service.
- **release**: the guard of `management`, or a `site_owner` session of the same site whose
  effective set holds `notes.edit`. This change issues no `site_owner` session, so only the first
  guard can pass. Edge refuses every other caller before it writes a command.
- **deploy**: added by SITE-026 (workspace change `add-site-gateway-hosting`). The guard of
  `management`, or a registered device of the kind `site_deployer` that was paired for the same
  site. A `site_deployer` of another site gets `403` `edge.capability_not_granted`, and every other
  device gets the stable forbidden error. Edge mints the site of the device as
  `x-ratatoskr-site-id`, and refuses a `site_deployer` session with the stable forbidden error on
  every route that is not of this class. The upload body of `.../deploys` streams under the
  `transfer` budget of ADR-0015; the bundle size limit belongs to the sites service.
- **site-origin**: the `public` class above. SITE-027 adds site owner sessions to this class.
- **sign-in**: `POST .../sessions` only (SITE-027). An anonymous request that the single-use
  PKCE code and its verifier authenticate, from the `Origin` of the approved hostname. It opens a
  `site_owner` session at the audience `site:<site id>` and sets the host-only cookie
  `__Host-ratatoskr-site` (`HttpOnly`, `Secure`, `SameSite=Strict`, `Path=/`). Edge reads that cookie
  only on the routes of its own site that take a site owner session, and an unsafe method there
  needs `X-Ratatoskr-Site-Request: 1` and the `Origin` of the approved hostname. Withdrawing the
  approval revokes the site's owner sessions in the same transaction.

The sites service is the second line. It refuses the anonymous claim on every route that is not
under `/v1/sites/{site}/content/`.

## Consequences

- The route table is the place where a new site route gets its caller. A row without a class stops
  Edge at startup, so the decision cannot be forgotten.
- Edge serves the approval routes and the build route itself, because the approval is an
  authorisation decision (`identity.site_approvals`) and only Edge publishes commands.
- An anonymous request costs Edge two bucket checks and one loopback request. It costs no session
  lookup and no database query in Edge.
- Each peer that is not the sites gateway has one client bucket, whatever header it sends. Behind
  the gateway, each value of `X-Ratatoskr-Client-IP` has one (SITE-026).

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
  `X-Ratatoskr-Client-IP`: done, see "The `public` class" above and the follow-up of ADR-0015.
- SITE-027 (`add-site-owner-editing-and-live`) issues `site_owner` sessions, adds the `sign-in`
  routes and the second guard of `release`.
- The two bucket sizes are constants in this change. They become validated configuration when an
  operator needs to change them.
