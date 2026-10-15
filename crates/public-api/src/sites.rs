//! `/v1/sites`: Edge in front of `ratatoskr-sites` (workspace change `add-site-publications`,
//! ADR-0017).
//!
//! The content routes, `/v1/sites/{site}/content` and below, are the `public` route class: `GET`
//! and `HEAD` only, no credential read, an anonymous claim, a bucket for each client and a budget
//! for each site, all decided before the proxy.

use std::sync::Arc;

use axum::extract::{Extension, FromRequestParts as _, Request, State};
use axum::response::Response;
use http::{HeaderValue, Method};
use platform_core::FailureKind;
use platform_http::ActorLimiter;
use uuid::Uuid;

use crate::{ApiState, Principal, gateway};

/// The name of the sites route in the gateway route table.
pub const SERVICE: &str = "sites";

/// The content route of a site, below `/v1/sites/`, and the prefix of every content route below it.
const CONTENT_PATH: &str = "{site}/content";

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
    /// The owner manages the site from the web client.
    Management,
    /// The site itself.
    SiteOrigin,
    /// The sign-in exchange of a site owner session.
    SignIn,
    /// The build request.
    Release,
}

/// One row of the site route table: the methods, the path below `/v1/sites/{site}`, and the class.
#[derive(Debug, Clone, Copy)]
pub struct SiteRoute {
    /// The methods of the row.
    pub methods: &'static [Method],
    /// The path below `/v1/sites/{site}`.
    pub path: &'static str,
    /// The class.
    pub class: Option<SiteRouteClass>,
}

/// Every route under `/v1/sites/{site}/`.
pub const ROUTES: &[SiteRoute] = &[];

/// The start-up check of a site route table.
///
/// # Errors
///
/// None yet.
pub const fn check_routes(_routes: &[SiteRoute]) -> Result<(), String> {
    Ok(())
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

/// Every request under `/v1/sites`. A content route is the `public` class and reads no credential;
/// every other route needs a session and goes through the generic proxy.
pub async fn dispatch(
    State(state): State<Arc<ApiState>>,
    Extension(context): Extension<platform_http::RequestContext>,
    request: Request,
) -> Response {
    let below = request.uri().path().strip_prefix("/v1/sites/");
    if below.is_some_and(|below| gateway::is_under(CONTENT_PATH, below)) {
        return content(State(state), Some(Extension(context)), request).await;
    }
    let (mut parts, body) = request.into_parts();
    let principal = match Principal::from_request_parts(&mut parts, &state).await {
        Ok(principal) => principal,
        Err(refusal) => return refusal,
    };
    gateway::proxy(
        State(state),
        principal,
        Extension(context),
        Request::from_parts(parts, body),
    )
    .await
}

/// `GET` and `HEAD /v1/sites/{site}/content/*`: the `public` class.
///
/// Every other method is `405`, and a request body is `400`. The buckets are checked before the
/// proxy, and a refusal is `429` with `Retry-After`. Nothing here reads a credential or a cookie.
pub async fn content(
    State(state): State<Arc<ApiState>>,
    context: Option<Extension<platform_http::RequestContext>>,
    request: Request,
) -> Response {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return platform_http::reject(FailureKind::MethodNotAllowed);
    }
    let headers = request.headers();
    let declares_body = headers
        .get(http::header::CONTENT_LENGTH)
        .is_some_and(|length| length.as_bytes() != b"0")
        || headers.contains_key(http::header::TRANSFER_ENCODING);
    if declares_body {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    let site = site_of(request.uri().path());
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
    let correlation = crate::correlation_of(context);
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", |value| value.as_str());
    state
        .gateway
        .forward_anonymous(SERVICE, method, path_and_query, headers, &correlation)
        .await
}

/// The site segment of a path below `/v1/sites/`.
fn site_of(path: &str) -> &str {
    path.strip_prefix("/v1/sites/")
        .and_then(|below| below.split('/').next())
        .unwrap_or_default()
}
