//! Helpers shared by the shutdown test binaries: listeners on ephemeral ports and a plain client.

#![allow(
    dead_code,
    reason = "each shutdown test binary uses a part of these helpers"
)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use http::{StatusCode, Uri};
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use platform_core::config::{PublicConfig, ShutdownConfig};
use platform_http::{HttpState, RuntimeState, Served, admin_router, public_router, serve};
use tokio::net::TcpListener;

/// A shutdown configuration written for a test rather than for a pod.
pub(crate) fn timing(drain_seconds: u64, grace_seconds: u64) -> ShutdownConfig {
    ShutdownConfig {
        drain_seconds,
        grace_seconds,
    }
}

/// The admin listener of a process that has bound everything, on an ephemeral port.
pub(crate) async fn admin(state: &Arc<RuntimeState>) -> (Served, Uri) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = serve(listener, admin_router(Arc::clone(state), String::new));
    (served, address_to_uri(address.to_string().as_str(), "/"))
}

/// The public listener of an edge process with one slow handler, on an ephemeral port.
pub(crate) async fn public(state: &Arc<HttpState>, handler_seconds: u64) -> (Served, Uri) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = PublicConfig {
        bind: address,
        request_timeout_seconds: 300,
        max_body_bytes: 1024,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    let routes = Router::new().route(
        "/slow",
        get(move || async move {
            tokio::time::sleep(Duration::from_secs(handler_seconds)).await;
            "done"
        }),
    );
    let served = serve(listener, public_router(Arc::clone(state), &config, routes));
    (
        served,
        address_to_uri(address.to_string().as_str(), "/slow"),
    )
}

pub(crate) fn address_to_uri(address: &str, path: &str) -> Uri {
    format!("http://{address}{path}").parse().unwrap()
}

pub(crate) fn with_path(base: &Uri, path: &str) -> Uri {
    address_to_uri(base.authority().unwrap().as_str(), path)
}

pub(crate) async fn status(uri: Uri) -> StatusCode {
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    client.get(uri).await.unwrap().status()
}
