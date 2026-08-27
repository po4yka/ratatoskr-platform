//! Streaming reverse proxy for configured loopback domain-service APIs.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::response::Response;
use http::header::{AUTHORIZATION, CONNECTION, COOKIE, HOST};
use http::{HeaderMap, HeaderName, HeaderValue, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use platform_core::FailureKind;
use platform_core::config::{
    GatewayConfig, GatewayRouteBudget, GatewayRouteBudgets, GatewayRouteConfig,
};
use ratatoskr_error_contracts::ErrorEnvelope;

use crate::{ApiState, Principal};

const RESERVED_PREFIX: &str = "x-ratatoskr-";
const MAX_ERROR_ENVELOPE_BYTES: usize = 65_536;

/// A service-owned document observed through its loopback capability endpoint.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ServiceCapabilities {
    /// Stable configured service name.
    pub service: String,
    /// The service's own capability document, opaque to Edge.
    #[schemars(with = "serde_json::Value")]
    pub document: serde_json::Value,
    /// RFC 3339 timestamp of the last successful observation, if one exists.
    pub observed_at: Option<String>,
    /// Whether the most recent refresh failed.
    pub stale: bool,
    /// RFC 3339 timestamp when the current stale period began, if stale.
    pub stale_since: Option<String>,
}

/// The reusable HTTP client and immutable route table for one Edge process.
#[derive(Clone)]
pub struct Gateway {
    client: Client<HttpConnector, Body>,
    routes: Arc<BTreeMap<String, GatewayRouteConfig>>,
    budgets: GatewayRouteBudgets,
    capabilities: Arc<tokio::sync::RwLock<BTreeMap<String, ServiceCapabilities>>>,
}

impl core::fmt::Debug for Gateway {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Gateway")
            .field("routes", &self.routes)
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// An empty gateway used by route tests and deployments without domain APIs.
    #[must_use]
    pub fn disabled() -> Self {
        Self::from_config(&GatewayConfig::default())
    }

    /// Build the one pooled loopback client for this process.
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> Self {
        let connector = HttpConnector::new();
        Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            routes: Arc::new(config.routes.clone()),
            budgets: config.budgets.clone(),
            capabilities: Arc::new(tokio::sync::RwLock::new(BTreeMap::new())),
        }
    }

    /// Whether at least one domain-service prefix is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.routes.is_empty()
    }

    /// The configured routes, in deterministic service-name order.
    #[must_use]
    pub fn routes(&self) -> &BTreeMap<String, GatewayRouteConfig> {
        &self.routes
    }

    /// The finite budget selected by a configured route.
    #[must_use]
    pub fn budget(&self, route: &GatewayRouteConfig) -> Option<GatewayRouteBudget> {
        route.class.map(|class| self.budgets.for_class(class))
    }

    /// Read the last sampled service documents without doing request-path fan-out.
    pub async fn capabilities(&self) -> Vec<ServiceCapabilities> {
        let snapshots = self.capabilities.read().await;
        self.routes
            .keys()
            .map(|service| {
                snapshots
                    .get(service)
                    .cloned()
                    .unwrap_or_else(|| ServiceCapabilities {
                        service: service.clone(),
                        document: serde_json::Value::Null,
                        observed_at: None,
                        stale: true,
                        stale_since: None,
                    })
            })
            .collect()
    }

    /// Refresh every configured service document on a bounded background cadence.
    pub async fn refresh_capabilities(&self) {
        for (service, route) in self.routes.iter() {
            let uri: Uri =
                match format!("http://{}{}", route.listener, route.capabilities_path).parse() {
                    Ok(uri) => uri,
                    Err(_) => continue,
                };
            let Ok(request) = hyper::Request::builder().uri(uri).body(Body::empty()) else {
                continue;
            };
            let document =
                match tokio::time::timeout(Duration::from_secs(5), self.client.request(request))
                    .await
                {
                    Ok(Ok(response)) if response.status().is_success() => {
                        match axum::body::to_bytes(
                            Body::new(response.into_body()),
                            MAX_ERROR_ENVELOPE_BYTES,
                        )
                        .await
                        {
                            Ok(bytes) => serde_json::from_slice(&bytes).ok(),
                            Err(_) => None,
                        }
                    }
                    _ => None,
                };
            let now = jiff::Timestamp::now().to_string();
            let mut snapshots = self.capabilities.write().await;
            if let Some(document) = document {
                snapshots.insert(
                    service.clone(),
                    ServiceCapabilities {
                        service: service.clone(),
                        document,
                        observed_at: Some(now),
                        stale: false,
                        stale_since: None,
                    },
                );
            } else {
                let previous = snapshots.get(service).cloned();
                snapshots.insert(
                    service.clone(),
                    ServiceCapabilities {
                        service: service.clone(),
                        document: previous
                            .as_ref()
                            .map_or(serde_json::Value::Null, |value| value.document.clone()),
                        observed_at: previous
                            .as_ref()
                            .and_then(|value| value.observed_at.clone()),
                        stale: true,
                        stale_since: previous.and_then(|value| value.stale_since).or(Some(now)),
                    },
                );
            }
        }
    }

    fn route(&self, path: &str) -> Option<&GatewayRouteConfig> {
        self.routes
            .values()
            .find(|route| path == route.prefix || path.starts_with(&format!("{}/", route.prefix)))
    }
}

/// Authenticate at Edge, mint bounded claims, and stream the request and response unchanged.
pub async fn proxy(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Extension(context): Extension<platform_http::RequestContext>,
    request: Request,
) -> Response {
    let Some(route) = state.gateway.route(request.uri().path()) else {
        return platform_http::reject(FailureKind::RouteNotFound);
    };
    let Some(budget) = state.gateway.budget(route) else {
        return platform_http::reject(FailureKind::UpstreamUnavailable);
    };
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let uri: Uri = match format!("http://{}{}", route.listener, path_and_query).parse() {
        Ok(uri) => uri,
        Err(_) => return platform_http::reject(FailureKind::UpstreamUnavailable),
    };
    let (parts, body) = request.into_parts();
    let Ok(mut upstream) = hyper::Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(body)
    else {
        return platform_http::reject(FailureKind::UpstreamUnavailable);
    };
    *upstream.headers_mut() = forwarded_headers(
        &parts.headers,
        principal,
        &context.correlation_id.to_string(),
    );
    match tokio::time::timeout(
        std::time::Duration::from_secs(budget.response_timeout_seconds),
        state.gateway.client.request(upstream),
    )
    .await
    {
        Ok(Ok(response)) => response_from_upstream(response).await,
        Ok(Err(_)) => {
            tracing::warn!("domain-service upstream could not be reached");
            platform_http::reject(FailureKind::UpstreamUnavailable)
        }
        Err(_) => {
            tracing::warn!("domain-service upstream response headers timed out");
            platform_http::reject(FailureKind::UpstreamTimeout)
        }
    }
}

/// Turn an upstream response into an Edge response without buffering successful or streaming
/// bodies. Error bodies are bounded and parsed because a downstream error is public only when it
/// is already the shared contract envelope.
async fn response_from_upstream(response: hyper::Response<hyper::body::Incoming>) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers = response_headers(&parts.headers);
    if !parts.status.is_client_error() && !parts.status.is_server_error() {
        return Response::from_parts(parts, Body::new(body));
    }

    let body = Body::new(body);
    let Ok(bytes) = axum::body::to_bytes(body, MAX_ERROR_ENVELOPE_BYTES).await else {
        return platform_http::reject(FailureKind::UpstreamInvalidResponse);
    };
    if serde_json::from_slice::<ErrorEnvelope>(&bytes).is_err() {
        return platform_http::reject(FailureKind::UpstreamInvalidResponse);
    }
    let response = Response::from_parts(parts, Body::from(bytes));
    platform_http::preserve_contract_error(response)
}

fn forwarded_headers(headers: &HeaderMap, principal: Principal, correlation_id: &str) -> HeaderMap {
    let connection_tokens = connection_tokens(headers);
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if name.as_str().starts_with(RESERVED_PREFIX)
            || connection_tokens.contains(name)
            || matches!(
                name,
                &CONNECTION
                    | &AUTHORIZATION
                    | &COOKIE
                    | &HOST
                    | &http::header::PROXY_AUTHENTICATE
                    | &http::header::PROXY_AUTHORIZATION
                    | &http::header::TE
                    | &http::header::TRAILER
                    | &http::header::TRANSFER_ENCODING
                    | &http::header::UPGRADE
            )
            || name == "keep-alive"
        {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    let user_id = principal.user_id.to_string();
    insert(&mut forwarded, "x-ratatoskr-user-id", &user_id);
    if let Some(device_id) = principal.device_id {
        let device_id = device_id.to_string();
        insert(&mut forwarded, "x-ratatoskr-device-id", &device_id);
    }
    insert(&mut forwarded, "x-correlation-id", correlation_id);
    forwarded
}

/// Remove hop-by-hop fields from a downstream response and prevent a domain service from minting
/// a header in Edge's reserved namespace. `Connection` can nominate arbitrary header names, so it
/// is parsed rather than treated as one fixed field.
fn response_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_tokens = connection_tokens(headers);
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if name.as_str().starts_with(RESERVED_PREFIX)
            || connection_tokens.contains(name)
            || matches!(
                name,
                &CONNECTION
                    | &http::header::PROXY_AUTHENTICATE
                    | &http::header::PROXY_AUTHORIZATION
                    | &http::header::TE
                    | &http::header::TRAILER
                    | &http::header::TRANSFER_ENCODING
                    | &http::header::UPGRADE
            )
            || name == "keep-alive"
        {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    forwarded
}

fn connection_tokens(headers: &HeaderMap) -> HashSet<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| HeaderName::from_bytes(value.trim().as_bytes()).ok())
        .collect()
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}
