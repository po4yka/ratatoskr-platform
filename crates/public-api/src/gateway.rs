//! Streaming reverse proxy for configured loopback domain-service APIs.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::{Body, HttpBody as _};
use axum::extract::{Extension, Request, State};
use axum::response::Response;
use http::{HeaderMap, HeaderValue, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use platform_core::FailureKind;
use platform_core::config::{
    GATEWAY_TRANSFER_PATHS, GatewayConfig, GatewayRouteBudget, GatewayRouteBudgets,
    GatewayRouteConfig,
};
use ratatoskr_ai_archive_contracts::platform_receipt;
use ratatoskr_error_contracts::ErrorEnvelope;

use crate::{ApiState, Principal};

mod headers;
mod public;

use headers::{archive_headers, forwarded_headers, insert, response_headers};

pub(crate) const MAX_ERROR_ENVELOPE_BYTES: usize = 65_536;

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

    fn route(&self, path: &str) -> Option<(&str, &GatewayRouteConfig)> {
        self.routes
            .iter()
            .find(|(_, route)| {
                path == route.prefix || path.starts_with(&format!("{}/", route.prefix))
            })
            .map(|(service, route)| (service.as_str(), route))
    }

    /// The budget of one request: the class of its route, or `transfer` under a transfer sub-path,
    /// with the smallest body limit of the sub-paths that hold the request.
    fn budget_for(
        &self,
        service: &str,
        route: &GatewayRouteConfig,
        path: &str,
    ) -> Option<GatewayRouteBudget> {
        let below = path.strip_prefix(route.prefix.as_str()).unwrap_or_default();
        GATEWAY_TRANSFER_PATHS
            .iter()
            .filter(|(owner, sub_path, _)| *owner == service && is_under(sub_path, below))
            .map(|(_, _, limit)| limit.unwrap_or(u64::MAX))
            .min()
            .map_or_else(
                || self.budget(route),
                |limit| {
                    let mut budget = self.budgets.transfer;
                    budget.max_body_bytes = budget.max_body_bytes.min(limit);
                    Some(budget)
                },
            )
    }

    /// Send a notes request for which Edge minted an operation (the connect and the write-mode
    /// switch) to the notes listener, with the client's method, path, body and headers unchanged,
    /// the minted claims, and the operation id. The answer is returned unread, so the caller
    /// decides what the client sees. `Err` is the failure to answer.
    ///
    /// Nothing here reads, buffers or logs the body: a connect body holds the Obsidian secrets.
    pub(crate) async fn send_notes_minted(
        &self,
        request: Request,
        claims: Claims<'_>,
        operation_id: uuid::Uuid,
    ) -> Result<hyper::Response<hyper::body::Incoming>, FailureKind> {
        let unavailable = FailureKind::UpstreamUnavailable;
        let route = self.routes.get("notes").ok_or(unavailable)?;
        let budget = self.budget(route).ok_or(unavailable)?;
        let path_and_query = request
            .uri()
            .path_and_query()
            .map_or("/", |value| value.as_str());
        let uri: Uri = format!("http://{}{}", route.listener, path_and_query)
            .parse()
            .map_err(|_| unavailable)?;
        let (parts, body) = request.into_parts();
        let mut upstream = hyper::Request::builder()
            .method(parts.method)
            .uri(uri)
            .body(body)
            .map_err(|_| unavailable)?;
        let mut headers = forwarded_headers(&parts.headers, claims);
        insert(
            &mut headers,
            platform_receipt::HEADER_OPERATION_ID,
            &operation_id.to_string(),
        );
        *upstream.headers_mut() = headers;
        match tokio::time::timeout(
            Duration::from_secs(budget.response_timeout_seconds),
            self.client.request(upstream),
        )
        .await
        {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) if caused_by::<http_body_util::LengthLimitError>(&error) => {
                tracing::warn!("a notes request body exceeded its budget while it streamed");
                Err(FailureKind::PayloadTooLarge)
            }
            Ok(Err(error)) if caused_by::<axum::Error>(&error) => {
                tracing::warn!("a notes request body failed while it streamed");
                Err(FailureKind::InvalidRequest)
            }
            Ok(Err(_)) => {
                tracing::warn!("the notes service could not be reached for a minted request");
                Err(unavailable)
            }
            Err(_) => {
                tracing::warn!("the notes service did not answer a minted request in time");
                Err(FailureKind::UpstreamTimeout)
            }
        }
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
        let claims = Claims {
            principal,
            device_kind: None,
            correlation_id,
        };
        let headers = forwarded_headers(&HeaderMap::new(), claims);
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
            Ok(Ok(response)) => response_from_upstream(response, provider).await,
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

/// The identity Edge mints onto one forwarded request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Claims<'a> {
    pub(crate) principal: Principal,
    /// The stored kind of the device, when the principal is a device.
    pub(crate) device_kind: Option<&'static str>,
    pub(crate) correlation_id: &'a str,
}

/// The stored kind of the authenticated device, or `None` when the principal is not a device.
///
/// A device that is gone, revoked or owned by someone else since its session authenticated is
/// refused as unauthenticated, and a read that fails is refused as a timeout.
pub(crate) async fn device_kind(
    state: &ApiState,
    principal: Principal,
) -> Result<Option<&'static str>, Response> {
    let Some(device_id) = principal.device_id else {
        return Ok(None);
    };
    match platform_identity::device::find_device(state.database.pool(), device_id).await {
        Ok(Some(device)) if device.user_id == principal.user_id && device.is_active() => {
            Ok(Some(device.kind.as_str()))
        }
        Ok(_) => Err(platform_http::reject(FailureKind::Unauthenticated)),
        Err(error) => {
            tracing::error!(%error, "the device kind could not be read");
            Err(platform_http::reject(FailureKind::RequestTimeout))
        }
    }
}

/// Whether `E` is in the source chain of a failed forward. A failure of the request body is there:
/// [`http_body_util::LengthLimitError`] when the body grew above its limit while it streamed (a body
/// that declares its length is refused before the forward starts), and [`axum::Error`] when the
/// body of the client failed, for example because the client stopped the upload.
fn caused_by<E: std::error::Error + 'static>(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(<dyn std::error::Error>::is::<E>)
}

/// Whether `path` is `pattern` or a path below it. A `{name}` segment of the pattern matches one
/// segment that is not empty.
pub(crate) fn is_under(pattern: &str, path: &str) -> bool {
    let mut segments = path.split('/');
    pattern.split('/').all(|expected| {
        segments.next().is_some_and(|segment| {
            if expected.starts_with('{') {
                !segment.is_empty()
            } else {
                segment == expected
            }
        })
    })
}

/// Authenticate at Edge, mint bounded claims, and stream the request and response unchanged.
pub async fn proxy(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Extension(context): Extension<platform_http::RequestContext>,
    request: Request,
) -> Response {
    let Some((service, route)) = state.gateway.route(request.uri().path()) else {
        return platform_http::reject(FailureKind::RouteNotFound);
    };
    let Some(budget) = state
        .gateway
        .budget_for(service, route, request.uri().path())
    else {
        return platform_http::reject(FailureKind::UpstreamUnavailable);
    };
    let device_kind = match device_kind(&state, principal).await {
        Ok(kind) => kind,
        Err(rejection) => return rejection,
    };
    let below = request.uri().path().strip_prefix(route.prefix.as_str());
    if service == "notes" && crate::notes::refuses(below.unwrap_or_default(), device_kind) {
        return platform_http::reject(FailureKind::Forbidden);
    }
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let uri: Uri = match format!("http://{}{}", route.listener, path_and_query).parse() {
        Ok(uri) => uri,
        Err(_) => return platform_http::reject(FailureKind::UpstreamUnavailable),
    };
    let (parts, body) = request.into_parts();
    // The route layer holds the limit of the route, and a sub-path can take less.
    if body.size_hint().lower() > budget.max_body_bytes {
        return platform_http::reject(FailureKind::PayloadTooLarge);
    }
    let limit = usize::try_from(budget.max_body_bytes).unwrap_or(usize::MAX);
    let body = Body::new(http_body_util::Limited::new(body, limit));
    let Ok(mut upstream) = hyper::Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(body)
    else {
        return platform_http::reject(FailureKind::UpstreamUnavailable);
    };
    *upstream.headers_mut() = forwarded_headers(
        &parts.headers,
        Claims {
            principal,
            device_kind,
            correlation_id: &context.correlation_id.to_string(),
        },
    );
    match tokio::time::timeout(
        std::time::Duration::from_secs(budget.response_timeout_seconds),
        state.gateway.client.request(upstream),
    )
    .await
    {
        Ok(Ok(response)) => response_from_upstream(response, &route.prefix).await,
        Ok(Err(error)) if caused_by::<http_body_util::LengthLimitError>(&error) => {
            tracing::warn!("a request body exceeded its budget while it streamed");
            platform_http::reject(FailureKind::PayloadTooLarge)
        }
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
///
/// A `401` or `407` is never relayed (XR-021 R3-03). Edge authenticated the caller before it
/// forwarded and sends no `Authorization` to a domain service, so an upstream authentication
/// challenge is a deployment or contract fault between Edge and that service. Relayed, it would
/// read to a client as its own credential being rejected, and the export agent removes its pairing
/// on exactly that. A redirect other than `304` is not relayed either: the client would follow it to
/// a place that Edge did not choose. `route` is the configured route prefix or the archive provider,
/// both fixed labels; the log carries only it and a class.
pub(crate) async fn response_from_upstream(
    response: hyper::Response<hyper::body::Incoming>,
    route: &str,
) -> Response {
    let (mut parts, body) = response.into_parts();
    if parts.status.is_redirection() && parts.status != StatusCode::NOT_MODIFIED {
        tracing::warn!(
            route,
            class = "upstream_redirect",
            "a domain service answered a redirect"
        );
        return platform_http::reject(FailureKind::UpstreamInvalidResponse);
    }
    if matches!(
        parts.status,
        StatusCode::UNAUTHORIZED | StatusCode::PROXY_AUTHENTICATION_REQUIRED
    ) {
        tracing::warn!(
            route,
            class = "upstream_unauthenticated",
            "a domain service answered an authentication challenge Edge never sent"
        );
        return platform_http::reject(FailureKind::UpstreamInvalidResponse);
    }
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
