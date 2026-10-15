//! `/v1/sites`: Edge in front of `ratatoskr-sites` (workspace change `add-site-publications`,
//! ADR-0017).
//!
//! `/v1/sites` and every route under `/v1/sites/{site}/` have one row in [`ROUTES`], and the row
//! has a class. The
//! class decides the caller: `management` and `release` need the owner, `deploy` needs the owner or
//! the `site_deployer` device of the site, and `site-origin` is the `public` class of the content
//! routes, which reads no credential. A request that no row holds is
//! `404` at Edge, and a row without a class stops Edge at start-up ([`check_routes`]).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, Extension, FromRequestParts as _, Request, State};
use axum::response::Response;
use http::{HeaderValue, Method};
use platform_core::FailureKind;
use platform_http::{ActorLimiter, RequestContext};
use platform_identity::SessionKind;
use ratatoskr_sites_contracts::SiteCapability;
use uuid::Uuid;

use crate::auth::{DeployRoute, DeployerSite};
use crate::{
    ApiState, Principal, gateway, site_approvals, site_capabilities, site_edits, site_events,
    site_releases, site_sessions,
};

/// The name of the sites route in the gateway route table.
pub const SERVICE: &str = "sites";

/// The approval route below `/v1/sites/`, which Edge serves itself (workspace spec `site-manifest`).
const APPROVAL_PATH: &str = "{site}/manifest/approval";

/// The sign-in code route below `/v1/sites/`, which Edge serves itself (workspace spec
/// `site-owner-session`).
const SIGN_IN_CODES_PATH: &str = "{site}/sign-in-codes";

/// The header that names the client of a public request. The sites gateway sets it, and Edge reads
/// it only on a connection from the gateway's address ([`client_key`]).
const CLIENT_HEADER: &str = "x-ratatoskr-client-ip";

/// The requests of one client in a minute, across every site (ADR-0017).
const CLIENT_REQUESTS_PER_MINUTE: u32 = 120;

/// The requests to one site in a minute, from every client (ADR-0017).
const SITE_REQUESTS_PER_MINUTE: u32 = 1200;

/// The namespaces of the bucket keys, so that a client and a site with the same text never share a
/// bucket.
const CLIENT_NAMESPACE: Uuid = Uuid::from_u128(0x0199_e5a0_0c11_7e47_9c0a_51e5_0000_0001);
const SITE_NAMESPACE: Uuid = Uuid::from_u128(0x0199_e5a0_0c11_7e47_9c0a_51e5_0000_0002);

/// The class of a route under `/v1/sites/{site}/` (ADR-0017). It decides who may call the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteRouteClass {
    /// The owner manages the site from the web client: a primary session with the live
    /// `platform.owner` grant.
    Management,
    /// The site itself. In this change, only the anonymous `public` class of the content routes.
    SiteOrigin,
    /// The sign-in exchange of a site owner session: an anonymous request that the PKCE code and
    /// verifier authenticate (workspace spec `site-owner-session`).
    SignIn,
    /// The build request: the guard of `Management`, or a `site_owner` session of the site with
    /// `notes.edit`. No route issues a `site_owner` session yet (SITE-027), so only the first
    /// guard can pass.
    Release,
    /// The upload and the activation of a deploy (workspace change `add-site-gateway-hosting`):
    /// the guard of `Management`, or a `site_deployer` device that was paired for the site.
    Deploy,
}

/// One row of the site route table: the methods, the path below `/v1/sites`, and the class.
///
/// A `{name}` segment of the path matches one segment that is not empty, and a last `{*name}`
/// segment matches one or more segments.
#[derive(Debug, Clone, Copy)]
pub struct SiteRoute {
    /// The methods of the row. A request to the path with another method is `405`.
    pub methods: &'static [Method],
    /// The path below `/v1/sites`: empty for the site list, else `/{site}/...`.
    pub path: &'static str,
    /// The class. A row without one stops Edge at start-up ([`check_routes`]).
    pub class: Option<SiteRouteClass>,
    /// The one capability of a `site-origin` row, which Edge checks against the site's effective
    /// set (workspace spec `site-capability-enforcement`). No other class has one.
    pub capability: Option<SiteCapability>,
}

/// `/v1/sites` and every route under `/v1/sites/{site}/` (workspace spec `site-route-classes`).
/// `GET /v1/sites` and `GET /v1/sites/{site}/manifest` are the reads of the web client (workspace
/// task 6.15); the sites service answers them ([`crate::site_reads`]).
pub const ROUTES: &[SiteRoute] = &[
    SiteRoute {
        methods: &[Method::GET],
        path: "",
        class: Some(SiteRouteClass::Management),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::GET, Method::PUT],
        path: "/{site}/manifest",
        class: Some(SiteRouteClass::Management),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::POST, Method::DELETE],
        path: "/{site}/manifest/approval",
        class: Some(SiteRouteClass::Management),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::GET, Method::PUT, Method::DELETE],
        path: "/{site}/publications",
        class: Some(SiteRouteClass::Management),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::GET, Method::PUT, Method::DELETE],
        path: "/{site}/publications/{publication}",
        class: Some(SiteRouteClass::Management),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/releases",
        class: Some(SiteRouteClass::Management),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/releases",
        class: Some(SiteRouteClass::Release),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/sign-in-codes",
        class: Some(SiteRouteClass::Management),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/sessions",
        class: Some(SiteRouteClass::SignIn),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/deploys",
        class: Some(SiteRouteClass::Deploy),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/deploys/{commit}/activate",
        class: Some(SiteRouteClass::Deploy),
        capability: None,
    },
    SiteRoute {
        methods: &[Method::GET, Method::HEAD],
        path: "/{site}/content/release",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::PagesRead),
    },
    SiteRoute {
        methods: &[Method::GET, Method::HEAD],
        path: "/{site}/content/pages/{*slug}",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::PagesRead),
    },
    SiteRoute {
        methods: &[Method::GET, Method::HEAD],
        path: "/{site}/content/assets/{sha256}",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::AssetsRead),
    },
    SiteRoute {
        methods: &[Method::GET, Method::HEAD],
        path: "/{site}/content/search",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::SearchPublication),
    },
    // The routes of the site owner session on the site origin (workspace change
    // `add-site-owner-editing-and-live`).
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/events",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::LiveRelease),
    },
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/drafts/pages/{*slug}",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::DraftRead),
    },
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/drafts/sources/{source_ref}",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::DraftRead),
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/drafts/sources/{source_ref}/blobs",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::NotesEdit),
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/edits",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::NotesEdit),
    },
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/conflicts/{conflict_id}",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::DraftRead),
    },
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/library",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::LibraryRead),
    },
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/knowledge/search",
        class: Some(SiteRouteClass::SiteOrigin),
        capability: Some(SiteCapability::KnowledgeSearch),
    },
];

/// The start-up check of a site route table: every row has a class, and no method and path has
/// two rows. Edge refuses to start on an `Err`.
///
/// # Errors
///
/// The text names each route that fails, as `METHOD /v1/sites/{site}/path`.
pub fn check_routes(routes: &[SiteRoute]) -> Result<(), String> {
    let mut failed = Vec::new();
    for (index, row) in routes.iter().enumerate() {
        for method in row.methods {
            let named = format!("{method} /v1/sites{}", row.path);
            if row.class.is_none() {
                failed.push(format!("{named} has no route class"));
            }
            let site_origin = row.class == Some(SiteRouteClass::SiteOrigin);
            if site_origin != row.capability.is_some() {
                failed.push(format!(
                    "{named} must name one capability exactly when it is site-origin"
                ));
            }
            let repeated = routes
                .iter()
                .skip(index + 1)
                .any(|other| other.path == row.path && other.methods.contains(method));
            if repeated {
                failed.push(format!("{named} has more than one row"));
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the site route table is not valid (ADR-0017): {}",
            failed.join("; ")
        ))
    }
}

/// The two token buckets of the `public` class: one for each client, keyed on the value of
/// `X-Ratatoskr-Client-IP`, and one for each site. Each table holds a bounded number of keys
/// ([`ActorLimiter`]).
#[derive(Debug)]
pub struct PublicLimits {
    clients: ActorLimiter,
    sites: ActorLimiter,
    retry_after_seconds: u32,
}

impl PublicLimits {
    /// Buckets of `per_client` and `per_site` requests a minute. A zero is read as one.
    #[must_use]
    pub fn new(per_client: u32, per_site: u32) -> Self {
        let slowest = per_client.min(per_site).max(1);
        Self {
            clients: ActorLimiter::new(per_client),
            sites: ActorLimiter::new(per_site),
            retry_after_seconds: 60_u32.div_ceil(slowest),
        }
    }

    /// Whether one more request of `client` to `site` is admitted. A request without a client
    /// header has the empty client, so every such request shares one bucket.
    fn admit(&self, client: &[u8], site: &str, now: jiff::Timestamp) -> bool {
        self.clients
            .admit(Uuid::new_v5(&CLIENT_NAMESPACE, client), now)
            && self
                .sites
                .admit(Uuid::new_v5(&SITE_NAMESPACE, site.as_bytes()), now)
    }
}

impl Default for PublicLimits {
    fn default() -> Self {
        Self::new(CLIENT_REQUESTS_PER_MINUTE, SITE_REQUESTS_PER_MINUTE)
    }
}

/// Every request under `/v1/sites`. The row of [`ROUTES`] that holds the path decides: no row is
/// `404`, a method that the row does not hold is `405`, `site-origin` is the `public` class,
/// `management` and `release` need the owner, and `deploy` needs the owner or the deployer device
/// of the site, before the request reaches the sites service.
pub async fn dispatch(
    State(state): State<Arc<ApiState>>,
    Extension(context): Extension<RequestContext>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let (site, class, capability) = match classify(&parts.method, parts.uri.path()) {
        Ok((
            site,
            SiteRoute {
                class: Some(class),
                capability,
                ..
            },
        )) => (site.to_owned(), *class, *capability),
        Ok(_) => return platform_http::reject(FailureKind::RouteNotFound),
        Err(refusal) => return platform_http::reject(refusal),
    };
    match class {
        SiteRouteClass::SiteOrigin => {
            let Some(capability) = capability else {
                return platform_http::reject(FailureKind::RouteNotFound);
            };
            site_origin(&state, &context, parts, body, &site, capability).await
        }
        SiteRouteClass::Management | SiteRouteClass::Release => {
            // A release from the site's page comes with the site cookie and no bearer.
            if class == SiteRouteClass::Release
                && !parts.headers.contains_key(http::header::AUTHORIZATION)
                && let Some(answer) = site_release(&state, &context, &parts, &site).await
            {
                return answer;
            }
            let principal = match owner(&state, &mut parts).await {
                Ok(principal) => principal,
                Err(refusal) => return refusal,
            };
            let correlation = context.correlation_id.to_string();
            if class == SiteRouteClass::Release {
                // Only Edge publishes commands, so Edge serves every route of this class.
                return site_releases::request_build(
                    &state,
                    principal,
                    &correlation,
                    &site,
                    &parts.headers,
                )
                .await;
            }
            let below = parts
                .uri
                .path()
                .strip_prefix("/v1/sites/")
                .unwrap_or_default();
            if gateway::is_under(SIGN_IN_CODES_PATH, below) {
                return site_sessions::issue_code(&state, principal, &context, &site, body).await;
            }
            if gateway::is_under(APPROVAL_PATH, below) {
                return if parts.method == Method::DELETE {
                    site_approvals::withdraw(&state, principal, &correlation, &site).await
                } else {
                    site_approvals::approve(&state, principal, &correlation, &site, body).await
                };
            }
            gateway::proxy(
                State(state),
                principal,
                Extension(context),
                Request::from_parts(parts, body),
            )
            .await
        }
        SiteRouteClass::Deploy => {
            let principal = match deployer(&state, &mut parts, &site).await {
                Ok(principal) => principal,
                Err(refusal) => return refusal,
            };
            // The activation takes no body, so it never streams under the transfer budget of the
            // upload that it sits below.
            if parts.uri.path().ends_with("/activate")
                && (declares_body(&parts.headers) || body.size_hint().exact() != Some(0))
            {
                return platform_http::reject(FailureKind::InvalidRequest);
            }
            gateway::proxy(
                State(state),
                principal,
                Extension(context),
                Request::from_parts(parts, body),
            )
            .await
        }
        SiteRouteClass::SignIn => {
            site_sessions::open_session(&state, &site, &parts.headers, body).await
        }
    }
}

/// Whether `path` is a route of the Content Delivery API, the `public` class of ADR-0017.
fn is_content(path: &str) -> bool {
    path.split('/').nth(4) == Some("content")
}

/// A `site-origin` route: the content routes are the `public` class; every other route needs its
/// capability in the site's effective set, and an owner-only capability also needs a site owner
/// session of the site (workspace spec `site-capability-enforcement`).
async fn site_origin(
    state: &Arc<ApiState>,
    context: &RequestContext,
    parts: http::request::Parts,
    body: axum::body::Body,
    site: &str,
    capability: SiteCapability,
) -> Response {
    if is_content(parts.uri.path()) {
        return content(state, context, site, &parts, capability).await;
    }
    let correlation = context.correlation_id.to_string();
    if capability.is_public() {
        // The one public route outside the content rows is the event stream. Every request to
        // it takes a token of the public rate limit first, the owner's too: the limit is per
        // client, and it comes before any read that the request causes.
        if let Some(refused) = public_limit(state, &parts, site) {
            return refused;
        }
        match site_capabilities::public(state, &correlation, site, capability).await {
            Ok(true) => {}
            Ok(false) => return site_sessions::site_not_found(context),
            Err(refusal) => return platform_http::reject(refusal),
        }
        let owner = match site_sessions::site_owner(state, &parts, site).await {
            Ok(principal) => principal.is_some(),
            Err(refusal) => return refusal,
        };
        let client = client_key(state, &parts);
        let connection = site_events::Connection {
            site,
            owner,
            client: &client,
            last_event_id: parts
                .headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok()),
        };
        return site_events::stream(&state.site_events, state.site_stream_limits, &connection);
    }
    // An owner-only capability: a site owner session of the site, whose effective set
    // holds the capability.
    let principal = match site_sessions::site_owner(state, &parts, site).await {
        Ok(Some(principal)) => principal,
        Ok(None) => return platform_http::reject(FailureKind::Unauthenticated),
        Err(refusal) => return refusal,
    };
    let granted =
        site_capabilities::require(state, principal, &correlation, site, capability.as_str()).await;
    if let Err(refusal) = granted {
        return platform_http::reject(refusal);
    }
    owner_route(state, context, parts, body, site, principal).await
}

/// A route of the site owner session, after the capability check.
async fn owner_route(
    state: &Arc<ApiState>,
    context: &RequestContext,
    parts: http::request::Parts,
    body: axum::body::Body,
    site: &str,
    principal: Principal,
) -> Response {
    let below: Vec<String> = parts
        .uri
        .path()
        .split('/')
        .skip(4)
        .map(str::to_owned)
        .collect();
    let below: Vec<&str> = below.iter().map(String::as_str).collect();
    match below.as_slice() {
        ["drafts", "sources", source_ref, "blobs"] => {
            site_edits::upload(state, context, principal, site, source_ref, body).await
        }
        ["drafts", ..] => {
            // The sites service answers the source of a note in scope.
            gateway::proxy(
                State(Arc::clone(state)),
                principal,
                Extension(context.clone()),
                Request::from_parts(parts, body),
            )
            .await
        }
        ["edits"] => site_edits::edit(state, context, principal, site, body).await,
        ["conflicts", conflict_id] => {
            site_edits::conflict(state, context, principal, site, conflict_id).await
        }
        // The owner's library and knowledge search, with the parameters and the answer of
        // `/v1/library/search`: the same Knowledge question for the same owner. A tenant
        // parameter is not a parameter of the route, so it is refused before Knowledge.
        ["library"] | ["knowledge", "search"] => {
            let params = axum::extract::Query::try_from_uri(&parts.uri);
            crate::library::search(State(Arc::clone(state)), principal, params).await
        }
        _ => platform_http::reject(FailureKind::RouteNotFound),
    }
}

/// The release route for a site owner session of `site` (the second guard of the class):
/// `None` when the request carries no such session.
async fn site_release(
    state: &Arc<ApiState>,
    context: &RequestContext,
    parts: &http::request::Parts,
    site: &str,
) -> Option<Response> {
    let principal = match site_sessions::site_owner(state, parts, site).await {
        Ok(Some(principal)) => principal,
        Ok(None) => return None,
        Err(refusal) => return Some(refusal),
    };
    let correlation = context.correlation_id.to_string();
    let granted = site_capabilities::require(
        state,
        principal,
        &correlation,
        site,
        site_capabilities::NOTES_EDIT,
    )
    .await;
    Some(match granted {
        Ok(()) => {
            site_releases::request_build(state, principal, &correlation, site, &parts.headers).await
        }
        Err(refusal) => platform_http::reject(refusal),
    })
}

/// The site and the class of the row that holds `method` and `path`.
fn classify<'a>(
    method: &Method,
    path: &'a str,
) -> Result<(&'a str, &'static SiteRoute), FailureKind> {
    let below = path
        .strip_prefix("/v1/sites")
        .ok_or(FailureKind::RouteNotFound)?;
    // The site segment, or nothing for `/v1/sites` itself.
    let site = below.split('/').nth(1).unwrap_or_default();
    let mut rows = ROUTES
        .iter()
        .filter(|row| holds(row.path, below))
        .peekable();
    if rows.peek().is_none() {
        return Err(FailureKind::RouteNotFound);
    }
    let row = rows
        .find(|row| row.methods.contains(method))
        .ok_or(FailureKind::MethodNotAllowed)?;
    Ok((site, row))
}

/// Whether `below`, the path below `/v1/sites`, is a path of the row `pattern`.
fn holds(pattern: &str, below: &str) -> bool {
    let mut actual = below.split('/');
    for expected in pattern.split('/') {
        let Some(segment) = actual.next() else {
            return false;
        };
        if expected.starts_with("{*") {
            return !segment.is_empty();
        }
        let matched = if expected.starts_with('{') {
            !segment.is_empty()
        } else {
            expected == segment
        };
        if !matched {
            return false;
        }
    }
    actual.next().is_none()
}

/// The caller of a `management` route, and of a `release` route in this change: a primary session
/// (one that a paired device did not open, ADR-0016) whose user holds the live owner grant.
async fn owner(
    state: &Arc<ApiState>,
    parts: &mut http::request::Parts,
) -> Result<Principal, Response> {
    let principal = Principal::from_request_parts(parts, state).await?;
    if principal.device_id.is_some() || principal.kind == SessionKind::Device {
        return Err(platform_http::reject(FailureKind::Forbidden));
    }
    crate::admin::require_owner(state, principal).await?;
    Ok(principal)
}

/// The caller of a `deploy` route: the caller of a `management` route, or a `site_deployer` device
/// that was paired for `site`. The device of another site gets `edge.capability_not_granted`, and
/// every other device gets the stable forbidden error.
async fn deployer(
    state: &Arc<ApiState>,
    parts: &mut http::request::Parts,
    site: &str,
) -> Result<Principal, Response> {
    parts.extensions.insert(DeployRoute);
    let principal = Principal::from_request_parts(parts, state).await?;
    if principal.device_id.is_none() && principal.kind != SessionKind::Device {
        crate::admin::require_owner(state, principal).await?;
        return Ok(principal);
    }
    // The extractor names the site of a `site_deployer`; no other device has one.
    match parts.extensions.get::<DeployerSite>() {
        Some(DeployerSite(own)) if own == site => Ok(principal),
        Some(_) => Err(platform_http::reject(FailureKind::CapabilityNotGranted)),
        None => Err(platform_http::reject(FailureKind::Forbidden)),
    }
}

/// The client key of a `public` request (workspace spec `site-gateway`): on a connection from the
/// sites gateway's address, the address in `X-Ratatoskr-Client-IP` when the request carries exactly
/// one value and it is an IP address; on every other connection, and for any other header, the peer
/// address. A request whose connection is not known has the empty key, so every such request shares
/// one bucket. The header never reaches the sites
/// service: the reserved `x-ratatoskr-` prefix is removed on every forward.
fn client_key(state: &ApiState, request: &http::request::Parts) -> String {
    let Some(ConnectInfo(peer)) = request.extensions.get::<ConnectInfo<SocketAddr>>() else {
        return String::new();
    };
    if peer.ip() == state.sites_gateway_peer {
        // Exactly one value, and an IP address. Anything else is not what the gateway sends.
        let mut values = request.headers.get_all(CLIENT_HEADER).iter();
        if let (Some(value), None) = (values.next(), values.next())
            && let Some(address) = value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<IpAddr>().ok())
        {
            return address.to_string();
        }
    }
    peer.ip().to_string()
}

/// Whether a request declares a body: a `Content-Length` that is not `0`, or a transfer coding.
fn declares_body(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_LENGTH)
        .is_some_and(|length| length.as_bytes() != b"0")
        || headers.contains_key(http::header::TRANSFER_ENCODING)
}

/// `GET` and `HEAD /v1/sites/{site}/content/*`: the `public` class.
///
/// A request that declares a body is `400`. The buckets are checked before the proxy, and a
/// refusal is `429` with `Retry-After`. Nothing here reads a credential or a cookie.
/// The refusal of a public request of the client of `request` to `site` above the public rate
/// limit, with its `Retry-After`; `None` when the request is admitted. Edge applies it before it
/// asks the sites service for the manifest.
fn public_limit(state: &ApiState, request: &http::request::Parts, site: &str) -> Option<Response> {
    let client = client_key(state, request);
    if state
        .site_limits
        .admit(client.as_bytes(), site, jiff::Timestamp::now())
    {
        return None;
    }
    let mut refused = platform_http::reject(FailureKind::RateLimited);
    refused.headers_mut().insert(
        http::header::RETRY_AFTER,
        HeaderValue::from(state.site_limits.retry_after_seconds),
    );
    Some(refused)
}

async fn content(
    state: &ApiState,
    context: &RequestContext,
    site: &str,
    request: &http::request::Parts,
    capability: SiteCapability,
) -> Response {
    let headers = &request.headers;
    if declares_body(headers) {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    if let Some(refused) = public_limit(state, request, site) {
        return refused;
    }
    let correlation = context.correlation_id.to_string();
    match site_capabilities::public(state, &correlation, site, capability).await {
        Ok(true) => {}
        Ok(false) => return site_sessions::site_not_found(context),
        Err(refusal) => return platform_http::reject(refusal),
    }
    let path_and_query = request
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    state
        .gateway
        .forward_anonymous(
            SERVICE,
            request.method.clone(),
            path_and_query,
            headers,
            &context.correlation_id.to_string(),
        )
        .await
}
