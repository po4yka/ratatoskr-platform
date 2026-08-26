//! The two credential exchanges a device drives itself: opening a session from its root secret,
//! and rotating an ongoing session's credentials.
//!
//! ADR-0016. The root secret authenticates exactly one thing — this login route — so it never
//! rides on ordinary requests. Refresh swaps BOTH credentials in one transaction and extends the
//! session's window; presenting a spent refresh link is evidence of a leak, and the whole family
//! burns for it.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{Method, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_identity::audit::{self, AuditEvent, AuditOutcome};
use platform_identity::session::{RotationFailure, rotate_session};
use platform_identity::{NewSession, SecretDigest, SessionKind};
use uuid::Uuid;

use platform_persistence::PersistenceError;

use crate::{ApiState, correlation_of};

/// How long each device-session credential lives before it must be refreshed.
const DEVICE_SESSION_TTL: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);

/// How long each refresh link lives.
const REFRESH_TTL: jiff::SignedDuration = jiff::SignedDuration::from_hours(24 * 30);

/// The longest body these routes will read before deciding anything.
const MAX_BODY: usize = 2048;

/// What a logging-in device presents.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct OpenDeviceSession {
    /// The device identifier issued at pairing.
    pub device_id: Uuid,
    /// The device's root secret. Stored only as a digest; wrong means refused.
    pub device_secret: String,
}

/// What a login grants — both credentials appear once, here, and nowhere again.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct DeviceSessionOpened {
    /// The internal user the device belongs to.
    pub user_id: Uuid,
    /// The device that authenticated.
    pub device_id: Uuid,
    /// The new session's bearer credential.
    pub credential: String,
    /// When that credential stops working.
    pub expires_at: String,
    /// The first link of the new refresh chain.
    pub refresh_token: String,
    /// When that link stops being usable.
    pub refresh_expires_at: String,
}

/// Read and bound a device-login body.
///
/// An unreadable body is a client error. An oversized one is attacker-chosen input on an
/// unauthenticated route: refused exactly like any other presentation that is not believed,
/// denial audited, reason kept out of the answer.
async fn parse_login(
    state: &ApiState,
    correlation: &str,
    body: &[u8],
) -> Result<OpenDeviceSession, Response> {
    if body.len() > MAX_BODY {
        return Err(refuse_device(state, correlation, None).await);
    }
    serde_json::from_slice::<OpenDeviceSession>(body)
        .map_err(|_| platform_http::reject(FailureKind::InvalidRequest))
}

/// `POST /v1/sessions/device`.
///
/// The recovery path: after every session of a device is revoked or expired, its root secret
/// opens a fresh one. A wrong secret, an unknown identifier and a revoked device are the same
/// 401 — three different answers would be an oracle (`ARCHITECTURE.md` S15) — and denials are
/// audited with the owning account when one can be attributed.
///
/// No `Idempotency-Key`: a retry mints another perfectly good session rather than replaying the
/// first answer, because sessions are cheap, bounded and revocable.
pub async fn open_device_session(
    State(state): State<Arc<ApiState>>,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let correlation = correlation_of(context);

    let request = match parse_login(&state, &correlation, &body).await {
        Ok(request) => request,
        Err(response) => return response,
    };

    let now = jiff::Timestamp::now();
    let presented = platform_identity::SecretDigest::of(&request.device_secret);
    let Ok(((access, access_digest), (refresh, refresh_digest))) = mint_pair() else {
        tracing::error!("a credential could not be minted");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    // One statement answers "active, correct secret, whose row" — and locks that row, so a
    // deletion committing concurrently makes this fail instead of granting onto a dying device.
    let owner = platform_identity::device::authenticate_device(
        &mut *transaction,
        request.device_id,
        presented,
    )
    .await;
    let owner = match owner {
        Ok(Some(user_id)) => user_id,
        Ok(None) => {
            drop(transaction);
            return refuse_device(&state, &correlation, Some(request.device_id)).await;
        }
        Err(error) => {
            tracing::error!(%error, "the device could not be verified");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let session = platform_identity::session::create_session(
        &mut *transaction,
        &NewSession {
            user_id: owner,
            kind: SessionKind::Device,
            device_id: Some(request.device_id),
            audience: &state.audience,
            token: Some(access_digest),
            issued_at: now,
            expires_at: now + DEVICE_SESSION_TTL,
        },
    )
    .await;
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(%error, "the device session could not be opened");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let link = platform_identity::session::issue_refresh_token(
        &mut *transaction,
        session.session_id,
        refresh_digest,
        now,
        now + REFRESH_TTL,
    )
    .await;
    let link = match link {
        Ok(link) => link,
        Err(error) => {
            tracing::error!(%error, "the refresh chain could not be started");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(owner),
        actor_session_id: Some(session.session_id),
        action: "session.open_device",
        target_kind: "session",
        target_id: Some(session.session_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.clone(),
    };
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "the device login could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the login transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    (
        http::StatusCode::CREATED,
        Json(DeviceSessionOpened {
            user_id: owner,
            device_id: request.device_id,
            credential: access,
            expires_at: session.expires_at.to_string(),
            refresh_token: refresh,
            refresh_expires_at: link.expires_at.to_string(),
        }),
    )
        .into_response()
}

/// What a refreshing client presents.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RefreshSession {
    /// The current, unspent link of the session's refresh chain.
    pub refresh_token: String,
}

/// What rotation returns — both credentials appear once, here, and nowhere again.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct RotatedCredentials {
    /// The replacement bearer credential for the same session.
    pub credential: String,
    /// When it stops working.
    pub expires_at: String,
    /// The next link of the chain. The presented one is spent from this instant.
    pub refresh_token: String,
    /// When that link stops being usable.
    pub refresh_expires_at: String,
}

/// `POST /v1/sessions/refresh`.
///
/// Spends the presented link, issues its successor, and swaps the session's bearer credential —
/// one transaction, so no crash leaves a session with two live credentials or none. Presenting a
/// SPENT link is replay evidence: the same refusal any other failure gives, plus the session's
/// family revoked on the spot, because a well-behaved client never replays.
///
/// No `Idempotency-Key`, deliberately: a retry after a lost response presents a now-spent token
/// and burns its own family — indistinguishable from theft by design. Recovery is one call to the
/// login route with the root secret, which is why that path exists.
pub async fn refresh(
    State(state): State<Arc<ApiState>>,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let correlation = correlation_of(context);

    if body.len() > MAX_BODY {
        return refuse_refresh(&state, &correlation).await;
    }
    let Ok(request) = serde_json::from_slice::<RefreshSession>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    if request.refresh_token.is_empty() || request.refresh_token.len() > 512 {
        return refuse_refresh(&state, &correlation).await;
    }

    let now = jiff::Timestamp::now();
    let presented = platform_identity::SecretDigest::of(&request.refresh_token);
    let Ok(((access, access_digest), (refresh, refresh_digest))) = mint_pair() else {
        tracing::error!("a credential could not be minted");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let outcome = rotate_session(
        &mut transaction,
        presented,
        refresh_digest,
        access_digest,
        now,
        now + DEVICE_SESSION_TTL,
        now + REFRESH_TTL,
    )
    .await;

    match outcome {
        Ok(Ok(rotated)) => {
            let event = AuditEvent {
                audit_event_id: Uuid::now_v7(),
                actor_user_id: None,
                actor_session_id: Some(rotated.refresh.session_id),
                action: "session.refresh",
                target_kind: "session",
                target_id: Some(rotated.refresh.session_id),
                outcome: AuditOutcome::Allowed,
                correlation_id: correlation.clone(),
            };
            if let Err(error) = audit::record(&mut *transaction, &event, now).await {
                tracing::error!(%error, "the rotation could not be audited");
                return platform_http::reject(FailureKind::RequestTimeout);
            }
            if let Err(error) = transaction.commit().await {
                tracing::error!(%error, "the rotation could not be committed");
                return platform_http::reject(FailureKind::RequestTimeout);
            }
            (
                http::StatusCode::OK,
                Json(RotatedCredentials {
                    credential: access,
                    expires_at: (now + DEVICE_SESSION_TTL).to_string(),
                    refresh_token: refresh,
                    refresh_expires_at: rotated.refresh.expires_at.to_string(),
                }),
            )
                .into_response()
        }
        Ok(Err(RotationFailure::Replayed { session_id, .. })) => {
            // The burn already happened inside the transaction; record the decision and commit it.
            let event = AuditEvent {
                audit_event_id: Uuid::now_v7(),
                actor_user_id: None,
                actor_session_id: Some(session_id),
                action: "session.refresh",
                target_kind: "session",
                target_id: Some(session_id),
                outcome: AuditOutcome::Denied,
                correlation_id: correlation.clone(),
            };
            if let Err(error) = audit::record(&mut *transaction, &event, now).await {
                tracing::error!(%error, "the replay could not be audited");
                return platform_http::reject(FailureKind::RequestTimeout);
            }
            if let Err(error) = transaction.commit().await {
                tracing::error!(%error, "the replay burn could not be committed");
                return platform_http::reject(FailureKind::RequestTimeout);
            }
            tracing::warn!(%session_id, "a refresh token was replayed; the family is burned");
            // The denial is already committed with the burn; refusing again would double-count it.
            platform_http::reject(FailureKind::Unauthenticated)
        }
        Ok(Err(_)) => {
            // Unknown, expired, dead session: nothing was written, one refusal fits all.
            drop(transaction);
            refuse_refresh(&state, &correlation).await
        }
        Err(error) => {
            tracing::error!(%error, "the rotation failed");
            platform_http::reject(FailureKind::RequestTimeout)
        }
    }
}

/// One refusal for every way a login is not believed.
async fn refuse_device(state: &ApiState, correlation: &str, attempted: Option<Uuid>) -> Response {
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: None,
        actor_session_id: None,
        action: "session.open_device",
        target_kind: "device",
        target_id: attempted,
        outcome: AuditOutcome::Denied,
        correlation_id: correlation.to_owned(),
    };
    if let Err(error) = audit::record(state.database.pool(), &event, jiff::Timestamp::now()).await {
        tracing::error!(%error, "a login denial could not be audited");
    }
    platform_http::reject(FailureKind::Unauthenticated)
}

/// One refusal for every way a refresh does not proceed.
async fn refuse_refresh(state: &ApiState, correlation: &str) -> Response {
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: None,
        actor_session_id: None,
        action: "session.refresh",
        target_kind: "refresh_token",
        target_id: None,
        outcome: AuditOutcome::Denied,
        correlation_id: correlation.to_owned(),
    };
    if let Err(error) = audit::record(state.database.pool(), &event, jiff::Timestamp::now()).await {
        tracing::error!(%error, "a refresh denial could not be audited");
    }
    platform_http::reject(FailureKind::Unauthenticated)
}

/// How the device-login route is described in the generated document.
pub const OPEN_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sessions/device",
    operation_id: "openDeviceSession",
    summary: "Open a session from a device's root secret",
    description: "\
The recovery login for a registered device: present your device identifier and root secret and \
receive a fresh session credential plus a new refresh chain, each returned once.\n\n\
A wrong secret, an unknown identifier and a revoked device receive the same refusal.",
    tag: "sessions",
    security: Security::None,
    parameters: &[],
    request: Some(Payload::Json("OpenDeviceSession")),
    responses: &[
        ResponseDoc {
            status: 201,
            description: "The session is open; its credentials appear once, here.",
            payload: Some(Payload::Json("DeviceSessionOpened")),
        },
        ResponseDoc {
            status: 400,
            description: "The body is not readable.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The presentation was not believed, for a reason this API does not disclose.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How the refresh route is described in the generated document.
pub const REFRESH_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sessions/refresh",
    operation_id: "refreshSession",
    summary: "Rotate a device session's credentials",
    description: "\
Exchanges the current refresh link for a new session credential AND the next link, atomically; \
the old bearer credential stops working at once. Each link works once.\n\n\
Presenting a spent link revokes the whole session — a well-behaved client never does that, so a \
replay means the credential leaked. The refusal looks like every other refresh failure; recover \
by logging in again with the device root secret.",
    tag: "sessions",
    security: Security::None,
    parameters: &[],
    request: Some(Payload::Json("RefreshSession")),
    responses: &[
        ResponseDoc {
            status: 200,
            description: "Both credentials are replaced; the answer carries them once.",
            payload: Some(Payload::Json("RotatedCredentials")),
        },
        ResponseDoc {
            status: 400,
            description: "The body is not readable.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The presented link was not usable, for a reason this API does not disclose.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was changed.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Mint an access credential and a refresh link in one breath.
///
/// # Errors
///
/// [`PersistenceError::Query`] carrying the generator's failure.
/// One minted credential: its plaintext, shown once, and the digest that gets stored.
type Minted = (String, SecretDigest);

/// Mint an access credential and a refresh link in one breath.
///
/// # Errors
///
/// [`PersistenceError::Query`] carrying the generator's failure.
fn mint_pair() -> Result<(Minted, Minted), PersistenceError> {
    let access = platform_identity::session::mint_credential()?;
    let refresh = platform_identity::session::mint_credential()?;
    Ok((access, refresh))
}
