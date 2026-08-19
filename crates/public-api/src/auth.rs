//! Turning a presented credential into a principal.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::response::Response;
use http::request::Parts;
use platform_core::FailureKind;
use platform_identity::{SecretDigest, SessionKind};
use sha2::{Digest as _, Sha256};
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

        let Some(presented) = bearer(parts) else {
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

/// The digest of the credential in `Authorization: Bearer …`, when there is one.
///
/// The scheme is matched case-insensitively, per RFC 9110 11.1. The credential itself is hashed
/// immediately and the original is never returned, so no caller can hold it.
fn bearer(parts: &Parts) -> Option<SecretDigest> {
    let value = parts
        .headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, credential) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let credential = credential.trim();
    if credential.is_empty() {
        return None;
    }
    Some(SecretDigest::new(
        Sha256::digest(credential.as_bytes()).into(),
    ))
}

/// Hash a credential the same way authentication does.
///
/// Public so a session can be minted with a digest that will later match. Without it a caller would
/// have to reproduce the hashing, and a mismatch would look like an authentication bug.
#[must_use]
pub fn credential_digest(credential: &str) -> SecretDigest {
    SecretDigest::new(Sha256::digest(credential.as_bytes()).into())
}
