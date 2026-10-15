//! `/v1/sites`: Edge in front of `ratatoskr-sites` (workspace change `add-site-publications`,
//! ADR-0017).
//!
//! `/v1/sites` and every route under `/v1/sites/{site}/` have one row in [`ROUTES`], and the row
//! has a class. The
//! class decides the caller: `management` and `release` need the owner, and `site-origin` is the
//! `public` class of the content routes, which reads no credential. A request that no row holds is
//! `404` at Edge, and a row without a class stops Edge at start-up ([`check_routes`]).

use std::sync::Arc;

use axum::extract::{Extension, FromRequestParts as _, Request, State};
use axum::response::Response;
use http::{HeaderValue, Method};
use platform_core::FailureKind;
use platform_http::{ActorLimiter, RequestContext};
use platform_identity::SessionKind;
use uuid::Uuid;

use crate::{ApiState, Principal, gateway, site_approvals, site_releases};

/// The name of the sites route in the gateway route table.
pub const SERVICE: &str = "sites";

/// The approval route below `/v1/sites/`, which Edge serves itself (workspace spec `site-manifest`).
const APPROVAL_PATH: &str = "{site}/manifest/approval";

/// The header that names the client of a public request. The sites gateway of SITE-026 sets it.
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
    /// The sign-in exchange of a site owner session. No route has it yet (SITE-027).
    SignIn,
    /// The build request: the guard of `Management`, or a `site_owner` session of the site with
    /// `notes.edit`. No route issues a `site_owner` session yet (SITE-027), so only the first
    /// guard can pass.
    Release,
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
}

/// `/v1/sites` and every route under `/v1/sites/{site}/` (workspace spec `site-route-classes`).
/// `GET /v1/sites` and `GET /v1/sites/{site}/manifest` are the reads of the web client (workspace
/// task 6.15); the sites service answers them ([`crate::site_reads`]).
pub const ROUTES: &[SiteRoute] = &[
    SiteRoute {
        methods: &[Method::GET],
        path: "",
        class: Some(SiteRouteClass::Management),
    },
    SiteRoute {
        methods: &[Method::GET, Method::PUT],
        path: "/{site}/manifest",
        class: Some(SiteRouteClass::Management),
    },
    SiteRoute {
        methods: &[Method::POST, Method::DELETE],
        path: "/{site}/manifest/approval",
        class: Some(SiteRouteClass::Management),
    },
    SiteRoute {
        methods: &[Method::GET, Method::PUT, Method::DELETE],
        path: "/{site}/publications",
        class: Some(SiteRouteClass::Management),
    },
    SiteRoute {
        methods: &[Method::GET, Method::PUT, Method::DELETE],
        path: "/{site}/publications/{publication}",
        class: Some(SiteRouteClass::Management),
    },
    SiteRoute {
        methods: &[Method::GET],
        path: "/{site}/releases",
        class: Some(SiteRouteClass::Management),
    },
    SiteRoute {
        methods: &[Method::POST],
        path: "/{site}/releases",
        class: Some(SiteRouteClass::Release),
    },
    SiteRoute {
        methods: &[Method::GET, Method::HEAD],
        path: "/{site}/content/{*rest}",
        class: Some(SiteRouteClass::SiteOrigin),
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
/// `404`, a method that the row does not hold is `405`, `site-origin` is the `public` class, and
/// `management` and `release` need the owner before the request reaches the sites service.
pub async fn dispatch(
    State(state): State<Arc<ApiState>>,
    Extension(context): Extension<RequestContext>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let (site, class) = match classify(&parts.method, parts.uri.path()) {
        Ok((site, class)) => (site.to_owned(), class),
        Err(refusal) => return platform_http::reject(refusal),
    };
    match class {
        SiteRouteClass::SiteOrigin => content(&state, &context, &site, &parts).await,
        SiteRouteClass::Management | SiteRouteClass::Release => {
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
        SiteRouteClass::SignIn => platform_http::reject(FailureKind::RouteNotFound),
    }
}

/// The site and the class of the row that holds `method` and `path`.
fn classify<'a>(method: &Method, path: &'a str) -> Result<(&'a str, SiteRouteClass), FailureKind> {
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
    let class = rows
        .find(|row| row.methods.contains(method))
        .ok_or(FailureKind::MethodNotAllowed)?
        .class
        .ok_or(FailureKind::RouteNotFound)?;
    Ok((site, class))
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

/// `GET` and `HEAD /v1/sites/{site}/content/*`: the `public` class.
///
/// A request that declares a body is `400`. The buckets are checked before the proxy, and a
/// refusal is `429` with `Retry-After`. Nothing here reads a credential or a cookie.
async fn content(
    state: &ApiState,
    context: &RequestContext,
    site: &str,
    request: &http::request::Parts,
) -> Response {
    let headers = &request.headers;
    let declares_body = headers
        .get(http::header::CONTENT_LENGTH)
        .is_some_and(|length| length.as_bytes() != b"0")
        || headers.contains_key(http::header::TRANSFER_ENCODING);
    if declares_body {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    let client = headers
        .get(CLIENT_HEADER)
        .map_or(&b""[..], HeaderValue::as_bytes);
    if !state
        .site_limits
        .admit(client, site, jiff::Timestamp::now())
    {
        let mut refused = platform_http::reject(FailureKind::RateLimited);
        refused.headers_mut().insert(
            http::header::RETRY_AFTER,
            HeaderValue::from(state.site_limits.retry_after_seconds),
        );
        return refused;
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
