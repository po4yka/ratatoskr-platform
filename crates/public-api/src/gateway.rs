//! Streaming reverse proxy for configured loopback domain-service APIs.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use ratatoskr_ai_archive_contracts::platform_receipt;
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

/// One operation-bound archive delivery after Edge has authenticated the device and read its
/// immutable receipt binding.
///
/// It carries the archive as a body and its declared size, and nothing about HTTP: the method, the
/// media type and the claim headers of the receipt binding are decided in one place,
/// [`Gateway::forward_archive_receipt`], so no caller can get one of them wrong.
pub(crate) struct ArchiveReceipt<'a> {
    pub(crate) provider: &'a str,
    pub(crate) principal: Principal,
    pub(crate) correlation_id: &'a str,
    pub(crate) operation_id: uuid::Uuid,
    pub(crate) sha256: &'a str,
    pub(crate) byte_size: u64,
    pub(crate) body: Body,
}

/// The reusable HTTP client and immutable route table for one Edge process.
#[derive(Clone)]
pub struct Gateway {
    client: Client<HttpConnector, Body>,
    routes: Arc<BTreeMap<String, GatewayRouteConfig>>,
    budgets: GatewayRouteBudgets,
    capabilities: Arc<tokio::sync::RwLock<BTreeMap<String, ServiceCapabilities>>>,
    knowledge_available: Arc<AtomicBool>,
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
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            routes: Arc::new(config.routes.clone()),
            budgets: config.budgets.clone(),
            capabilities: Arc::new(tokio::sync::RwLock::new(BTreeMap::new())),
            knowledge_available: Arc::new(AtomicBool::new(false)),
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

    /// The fixed transfer budget used by the operation-bound binary receipt endpoints.
    #[must_use]
    pub const fn transfer_budget(&self) -> GatewayRouteBudget {
        self.budgets.transfer
    }

    /// The finite control-plane response budget used by dedicated typed clients.
    #[must_use]
    pub(crate) const fn control_budget(&self) -> GatewayRouteBudget {
        self.budgets.control
    }

    /// A configured service listener, available only to fixed-path typed clients in this crate.
    #[must_use]
    pub(crate) fn service_listener(&self, service: &str) -> Option<std::net::SocketAddr> {
        self.routes.get(service).map(|route| route.listener)
    }

    /// Execute one fixed-path typed control request under the configured response-header budget.
    ///
    /// `dependency` names the service in the log line; it is a fixed label and never a value taken
    /// from a request.
    async fn request_control(
        &self,
        dependency: &'static str,
        request: hyper::Request<Body>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, FailureKind> {
        match tokio::time::timeout(
            Duration::from_secs(self.budgets.control.response_timeout_seconds),
            self.client.request(request),
        )
        .await
        {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                tracing::warn!(
                    dependency,
                    class = "unavailable",
                    "typed dependency request failed"
                );
                Err(FailureKind::UpstreamUnavailable)
            }
            Err(_) => {
                tracing::warn!(
                    dependency,
                    class = "timeout",
                    "typed dependency response headers timed out"
                );
                Err(FailureKind::UpstreamTimeout)
            }
        }
    }

    /// Send one typed control request and read its JSON answer, bounded in time and size.
    ///
    /// The one place a typed client (Knowledge, channel-digests) turns an HTTP answer into a value:
    /// a `404` is the caller's `NotFound` when `scoped_not_found` says the route is scoped to the
    /// caller, any other non-2xx answer, an answer above `max_response_bytes` and a body that is not
    /// the expected type are all `UpstreamInvalidResponse`, a transport failure is
    /// `UpstreamUnavailable`, and the control budget's deadline is `UpstreamTimeout`. Redirects are
    /// not followed.
    pub(crate) async fn fetch_json<T: serde::de::DeserializeOwned>(
        &self,
        dependency: &'static str,
        request: hyper::Request<Body>,
        scoped_not_found: bool,
        max_response_bytes: usize,
    ) -> Result<T, FailureKind> {
        let budget = self.control_budget();
        let max_body = usize::try_from(budget.max_body_bytes)
            .unwrap_or(usize::MAX)
            .min(max_response_bytes);
        tokio::time::timeout(
            Duration::from_secs(budget.response_timeout_seconds),
            async {
                let response = self.request_control(dependency, request).await?;
                if scoped_not_found && response.status() == http::StatusCode::NOT_FOUND {
                    return Err(FailureKind::NotFound);
                }
                if !response.status().is_success() {
                    tracing::warn!(
                        dependency,
                        class = "invalid_status",
                        "typed dependency returned an unusable status"
                    );
                    return Err(FailureKind::UpstreamInvalidResponse);
                }
                let body = axum::body::to_bytes(Body::new(response.into_body()), max_body)
                    .await
                    .map_err(|_| {
                        tracing::warn!(
                            dependency,
                            class = "oversized_body",
                            "typed dependency response exceeded its bound"
                        );
                        FailureKind::UpstreamInvalidResponse
                    })?;
                serde_json::from_slice(&body).map_err(|_| {
                    tracing::warn!(
                        dependency,
                        class = "invalid_json",
                        "typed dependency returned an invalid success body"
                    );
                    FailureKind::UpstreamInvalidResponse
                })
            },
        )
        .await
        .map_err(|_| {
            tracing::warn!(
                dependency,
                class = "total_timeout",
                "typed dependency total deadline elapsed"
            );
            FailureKind::UpstreamTimeout
        })?
    }

    /// Whether a configured transfer-class receiver can accept an archive for this provider.
    #[must_use]
    pub fn has_archive_receiver(&self, provider: &str) -> bool {
        self.routes.get(provider).is_some_and(|route| {
            route.class == Some(platform_core::config::GatewayRouteClass::Transfer)
        })
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

    /// Whether the receiver for `provider` can take an archive.
    ///
    /// True only when the last probe was fresh AND the document it returned says this listener
    /// serves the archive receipt for this route (`platform_receipt::is_receipt_capability_document`).
    /// A fresh probe alone proves that something answered on the port; the document proves it is the
    /// receiver. Anything else on that port, an empty document, or the other provider's document
    /// leaves archive acceptance closed rather than accepting uploads Edge cannot deliver.
    pub async fn archive_receiver_available(&self, provider: &str) -> bool {
        self.capabilities
            .read()
            .await
            .get(provider)
            .is_some_and(|snapshot| {
                !snapshot.stale
                    && platform_receipt::is_receipt_capability_document(
                        &snapshot.document,
                        provider,
                    )
            })
    }

    /// Whether the last background Knowledge observation succeeded.
    #[must_use]
    pub fn knowledge_available(&self) -> bool {
        self.knowledge_available.load(Ordering::Acquire)
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
            if service == "knowledge" {
                self.knowledge_available.store(
                    document.as_ref().is_some_and(knowledge_surface_available),
                    Ordering::Release,
                );
            }
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

    /// Stream a prepared archive to the receiving service's fixed receipt endpoint.
    ///
    /// The binding is `POST` with `Content-Type: application/zip` and a `Content-Length` equal to
    /// the declared size, plus the claims Edge mints (CONTRACTS.md S06 D1). The caller cannot choose
    /// its destination, its method or its operation identity: Edge looks the destination and the
    /// identity up from durable preparation metadata and injects them on a header map that starts
    /// empty, so nothing a client sent can ride along.
    pub(crate) async fn forward_archive_receipt(&self, receipt: ArchiveReceipt<'_>) -> Response {
        let ArchiveReceipt {
            provider,
            principal,
            correlation_id,
            operation_id,
            sha256,
            byte_size,
            body,
        } = receipt;
        let Some(route) = self.routes.get(provider) else {
            return platform_http::reject(FailureKind::UpstreamUnavailable);
        };
        let Some(budget) = self.budget(route) else {
            return platform_http::reject(FailureKind::UpstreamUnavailable);
        };
        let uri: Uri =
            match format!("http://{}{}", route.listener, route.archive_receipt_path).parse() {
                Ok(uri) => uri,
                Err(_) => return platform_http::reject(FailureKind::UpstreamUnavailable),
            };
        let Ok(mut upstream) = hyper::Request::builder()
            .method(http::Method::POST)
            .uri(uri)
            .body(body)
        else {
            return platform_http::reject(FailureKind::UpstreamUnavailable);
        };
        let headers = forwarded_headers(&HeaderMap::new(), principal, correlation_id);
        let mut headers = archive_headers(headers, operation_id, sha256, byte_size);
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static(platform_receipt::ARCHIVE_MEDIA_TYPE),
        );
        headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(byte_size));
        *upstream.headers_mut() = headers;
        match tokio::time::timeout(
            Duration::from_secs(budget.response_timeout_seconds),
            self.client.request(upstream),
        )
        .await
        {
            Ok(Ok(response)) => response_from_upstream(response).await,
            Ok(Err(_)) => {
                tracing::warn!(provider, "archive importer could not be reached");
                platform_http::reject(FailureKind::UpstreamUnavailable)
            }
            Err(_) => {
                tracing::warn!(provider, "archive importer response headers timed out");
                platform_http::reject(FailureKind::UpstreamTimeout)
            }
        }
    }
}

fn knowledge_surface_available(document: &serde_json::Value) -> bool {
    if document.get("service").and_then(serde_json::Value::as_str) != Some("knowledge") {
        return false;
    }
    let Some(capabilities) = document
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    ["library.search", "library.read_state"]
        .into_iter()
        .all(|required| {
            capabilities
                .iter()
                .any(|value| value.as_str() == Some(required))
        })
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
    insert(&mut forwarded, platform_receipt::HEADER_USER_ID, &user_id);
    if let Some(device_id) = principal.device_id {
        let device_id = device_id.to_string();
        insert(
            &mut forwarded,
            platform_receipt::HEADER_DEVICE_ID,
            &device_id,
        );
    }
    insert(
        &mut forwarded,
        platform_receipt::HEADER_CORRELATION_ID,
        correlation_id,
    );
    forwarded
}

fn archive_headers(
    mut headers: HeaderMap,
    operation_id: uuid::Uuid,
    sha256: &str,
    byte_size: u64,
) -> HeaderMap {
    insert(
        &mut headers,
        platform_receipt::HEADER_OPERATION_ID,
        &operation_id.to_string(),
    );
    insert(
        &mut headers,
        platform_receipt::HEADER_ARCHIVE_SHA256,
        sha256,
    );
    insert(
        &mut headers,
        platform_receipt::HEADER_ARCHIVE_BYTE_SIZE,
        &byte_size.to_string(),
    );
    headers
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
