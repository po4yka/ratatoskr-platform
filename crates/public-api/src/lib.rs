//! The versioned public API.
//!
//! Milestone 5, and the first thing in this repository that a client can call. It is where the four
//! earlier milestones meet: a request authenticates against `identity`, reserves an idempotency key,
//! creates an `operations` record and enqueues a command into the outbox — the last three in ONE
//! transaction, which is the whole point of building them that way.
//!
//! Two rules govern every handler here.
//!
//! **A handler never authors an error body.** It returns [`platform_http::reject`] with a named
//! [`FailureKind`], and the one middleware renders the envelope. There is still exactly one
//! `ErrorEnvelope` construction site in the repository (test F-1).
//!
//! **Authorization comes before existence.** `ARCHITECTURE.md` S15. An operation that belongs to
//! someone else and an operation that does not exist produce the same 404, so the API is not an
//! oracle for which identifiers are real.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use platform_persistence::Database;

pub mod auth;
pub mod captures;
pub mod operations;
pub mod stream;

pub use crate::auth::Principal;

/// What every handler needs.
#[derive(Debug, Clone)]
pub struct ApiState {
    /// The pool. `identity` and `operations` only.
    pub database: Database,
    /// The audience this listener serves. A session issued for another audience does not
    /// authenticate here, which is what stops a token minted for one surface being replayed at
    /// another (`SECURITY.md`: validate issuer/audience/expiry/nonce).
    pub audience: String,
    /// How long an idempotency key is honoured.
    pub idempotency_ttl: jiff::SignedDuration,
}

impl ApiState {
    /// The default replay window: 24 hours.
    ///
    /// Long enough to cover a client retrying after an outage, short enough that the ledger is not a
    /// permanent record of every request ever made.
    #[must_use]
    pub fn new(database: Database, audience: impl Into<String>) -> Self {
        Self {
            database,
            audience: audience.into(),
            idempotency_ttl: jiff::SignedDuration::from_hours(24),
        }
    }
}

/// The public routes.
///
/// Every path carries its major version. `ARCHITECTURE.md` S5.3: versioned `/v2` resource-oriented
/// routes. The version is 2 because Ratatoskr Next is the second system to serve this surface, and
/// starting at `/v1` would collide with the retired one in any client that ever spoke to both.
pub fn routes(state: ApiState) -> Router {
    let state = Arc::new(state);
    Router::new()
        .route("/v2/captures", post(captures::submit))
        .route("/v2/operations/{operation_id}", get(operations::read))
        .route("/v2/operations/{operation_id}/events", get(stream::events))
        .with_state(state)
}
