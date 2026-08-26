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

        // After authentication, because until then there is no actor to charge. Behind a tunnel
        // that adds its own headers there is no client identity a process may trust
        // (`ARCHITECTURE.md` S15), so a limit applied earlier would be a limit on the tunnel.
        //
        // Here rather than in each handler: this extractor runs on every authenticated route, so one
        // check covers the surface and the next route added is covered by construction.
        if !state
            .actor_limit
            .admit(session.user_id, jiff::Timestamp::now())
        {
            return Err(platform_http::reject(FailureKind::RateLimited));
        }

        // Liveness, best-effort and throttled to one write per interval per session (ADR-0016).
        // It runs AFTER admission because it exists to make admitted traffic visible; a failure
        // here is logged and never fails a request authentication already allowed.
        let touched = platform_identity::session::touch_last_seen(
            state.database.pool(),
            session.session_id,
            session.device_id,
            jiff::Timestamp::now(),
            LAST_SEEN_INTERVAL,
        )
        .await;
        if let Err(error) = touched {
            tracing::debug!(%error, "the liveness touch could not be written");
        }

        Ok(Self {
            user_id: session.user_id,
            session_id: session.session_id,
            kind: session.kind,
        })
    }
}

/// How often an authenticated session's last-seen instant may move. One write per minute per
/// active session is the worst case on the single host, and no settings page reads it finer.
const LAST_SEEN_INTERVAL: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);

/// Hash a credential the same way authentication does.
///
/// Public so a session can be minted with a digest that will later match. It is
/// [`SecretDigest::of`] under a name that says what it is for at the call site; there is still one
/// implementation.
#[must_use]
pub fn credential_digest(credential: &str) -> SecretDigest {
    SecretDigest::of(credential)
}
