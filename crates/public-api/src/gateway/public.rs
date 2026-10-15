//! The forward of the `public` route class (ADR-0017): no credential, the anonymous claim, and the
//! answer of the domain service without a cookie.

use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use http::{HeaderMap, Method, Uri};
use platform_core::FailureKind;

use super::headers::anonymous_headers;
use super::{Gateway, response_from_upstream};

impl Gateway {
    /// Send one anonymous `GET` or `HEAD` to the listener of `service`, with no body, and return its
    /// answer.
    ///
    /// The caller has checked the method and the rate limits. The request carries the anonymous
    /// claim and no credential, cookie or reserved header of the client. The answer keeps the
    /// headers of the service, so the service owns `Cache-Control` and `ETag`, except `Set-Cookie`:
    /// a shared cache must never store a cookie with a public answer.
    pub(crate) async fn forward_anonymous(
        &self,
        service: &'static str,
        method: Method,
        path_and_query: &str,
        headers: &HeaderMap,
        correlation_id: &str,
    ) -> Response {
        let unavailable = || platform_http::reject(FailureKind::UpstreamUnavailable);
        let Some(route) = self.routes.get(service) else {
            return unavailable();
        };
        let Some(budget) = self.budget(route) else {
            return unavailable();
        };
        let path = path_and_query.split('?').next().unwrap_or_default();
        if !super::paths::forwardable(&route.prefix, path) {
            return platform_http::reject(FailureKind::RouteNotFound);
        }
        let Ok(uri) = format!("http://{}{path_and_query}", route.listener).parse::<Uri>() else {
            return unavailable();
        };
        let Ok(mut upstream) = hyper::Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
        else {
            return unavailable();
        };
        *upstream.headers_mut() = anonymous_headers(headers, correlation_id);
        match tokio::time::timeout(
            Duration::from_secs(budget.response_timeout_seconds),
            self.client.request(upstream),
        )
        .await
        {
            Ok(Ok(response)) => {
                let mut response = response_from_upstream(response, &route.prefix).await;
                response.headers_mut().remove(http::header::SET_COOKIE);
                response
            }
            Ok(Err(_)) => {
                tracing::warn!(service, "a public request could not reach its service");
                unavailable()
            }
            Err(_) => {
                tracing::warn!(
                    service,
                    "a public request timed out waiting for its service"
                );
                platform_http::reject(FailureKind::UpstreamTimeout)
            }
        }
    }
}
