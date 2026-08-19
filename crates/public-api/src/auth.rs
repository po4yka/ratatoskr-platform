//! Turning a presented credential into a principal.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::response::Response;
use http::request::Parts;
use platform_core::FailureKind;
use platform_identity::{SecretDigest, SessionKind};
use uuid::Uuid;

use crate::ApiState;

/// Who is making the request.
///
/// Carries the identifiers a handler needs and nothing else. In particular it carries no credential:
/// once authentication is done the credential has no further use, and a handler that cannot reach it
/// cannot log it.
#[derive(Debug, Clone, Copy)]
pub struct Principal {
    /// The internal user.
    pub user_id: Uuid,
    /// The session that authenticated.
    pub session_id: Uuid,
    /// How that session was established.
    pub kind: SessionKind,
}

impl FromRequestParts<Arc<ApiState>> for Principal {
    type Rejection = Response;

    /// Authenticate, or refuse.
    ///
    /// Every refusal is the same [`FailureKind::Unauthenticated`]: a missing header, a malformed
    /// one, an unknown credential, a revoked session, an expired session and a session for another
    /// audience must be indistinguishable from outside, or the difference is an oracle
    /// (`ARCHITECTURE.md` S15).
    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ApiState>,
    ) -> Result<Self, Self::Rejection> {
        let refuse = || platform_http::reject(FailureKind::Unauthenticated);

        let Some(presented) = platform_identity::bearer(&parts.headers) else {
            return Err(refuse());
        };

        let session = platform_identity::session::authenticate(
            state.database.pool(),
            presented,
            &state.audience,
            jiff::Timestamp::now(),
        )
        .await
        .map_err(|error| {
            // A database failure is not an authentication failure, and must not be reported as one:
            // telling a caller "unauthenticated" when the database is down sends them to rotate a
            // credential that was never the problem.
            tracing::error!(%error, "authentication could not be completed");
            platform_http::reject(FailureKind::RequestTimeout)
        })?;

        let Some(session) = session else {
            return Err(refuse());
        };

        Ok(Self {
            user_id: session.user_id,
            session_id: session.session_id,
            kind: session.kind,
        })
    }
}

/// Hash a credential the same way authentication does.
///
/// Public so a session can be minted with a digest that will later match. It is
/// [`SecretDigest::of`] under a name that says what it is for at the call site; there is still one
/// implementation.
#[must_use]
pub fn credential_digest(credential: &str) -> SecretDigest {
    SecretDigest::of(credential)
}
