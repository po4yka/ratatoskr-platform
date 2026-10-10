//! The pieces of the capture route tests that more than one test binary needs: an Edge over a test
//! database, the real public pipeline, a user with a live session of a chosen kind, and `send`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::PublicConfig;
use platform_http::HttpState;
use platform_identity::{NewSession, SessionKind};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::{ApiState, auth};
use tower::ServiceExt as _;
use uuid::Uuid;

pub(crate) const CREDENTIAL: &str = "a-test-session-credential";
pub(crate) const AUDIENCE: &str = "edge";

/// The handlers read the wall clock themselves — the API state carries no clock — so a session
/// or grant seeded here has to be minted on that same clock to be live when the handler checks it.
pub(crate) fn now() -> jiff::Timestamp {
    jiff::Timestamp::now() // wall-clock: the handlers under test read the clock themselves
}

/// An `ApiState` with a healthy database and a configured bus.
///
/// The two facts `GET /v1/capabilities` reads. Every test here exercises a route that needs both,
/// so the default is "the deployment is whole"; the capability tests are where the other
/// combinations live.
pub(crate) fn state(harness: &TestDatabase) -> ApiState {
    let health = Arc::new(platform_http::RuntimeState::new(RuntimeRole::Edge));
    health.set_database_reachable(true);
    ApiState::new(harness.database.clone(), AUDIENCE, health, true)
}

/// The real public pipeline, not a bare router: the middleware is what renders an authored failure
/// into an `ErrorEnvelope`, so a test without it would assert statuses and prove nothing about
/// bodies.
pub(crate) fn app(state: ApiState) -> Router {
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 15,
        max_body_bytes: 1_048_576,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Edge)),
        &config,
        platform_public_api::routes(std::sync::Arc::new(state)),
    )
}

/// A user whose live session is of `kind`. A `Device` session needs a registered device.
pub(crate) async fn seed_session(
    pool: &sqlx::PgPool,
    credential: &str,
    audience: &str,
    kind: SessionKind,
) -> Uuid {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let device_id = if kind == SessionKind::Device {
        Some(
            platform_identity::device::register_device(
                pool,
                user.user_id,
                platform_identity::DeviceKind::ExportAgent,
                None,
                platform_identity::SecretDigest::of("device-secret"),
                now(),
            )
            .await
            .expect("a device")
            .device_id,
        )
    } else {
        None
    };
    platform_identity::session::create_session(
        pool,
        &NewSession {
            user_id: user.user_id,
            kind,
            device_id,
            audience,
            token: Some(auth::credential_digest(credential)),
            issued_at: now(),
            expires_at: now() + jiff::SignedDuration::from_hours(1),
        },
    )
    .await
    .expect("a session");
    user.user_id
}

pub(crate) async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}
