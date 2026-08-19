//! Exchanging what another service says about a person for a session here.
//!
//! `ARCHITECTURE.md` S6.3: `ratatoskr-telegram` validates raw Mini App `initData` because it owns the
//! bot token, and returns a short-lived signed assertion. Edge exchanges that assertion for a
//! short-lived Platform session. Platform never receives the bot token — and, per ADR-0011, holds
//! only the public half of the issuer's key, so it can verify an assertion and cannot issue one.
//!
//! The route is unauthenticated by definition: it is how a caller becomes authenticated. Everything
//! it accepts is therefore bounded, and every way of failing produces one indistinguishable refusal.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{Method, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_identity::{
    AuditEvent, AuditOutcome, IdentityProvider, NewSession, SessionKind, assertion, audit, session,
    user,
};
use uuid::Uuid;

use crate::ApiState;

/// How long a session minted from an assertion lives.
///
/// One hour. `ARCHITECTURE.md` S6.2 calls this session type short-lived, and the Mini App is a
/// surface a person opens, uses and closes; a longer life would only widen the window a stolen
/// credential is useful in, since nothing here refreshes.
const SESSION_LIFETIME: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);

/// What a client presents.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExchangeAssertion {
    /// The compact assertion issued by `ratatoskr-telegram`: `base64url(payload).base64url(signature)`.
    pub assertion: String,
}

/// The session, once.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SessionMinted {
    /// The bearer credential. Returned exactly once — only its digest is stored, so it cannot be
    /// recovered from Platform afterwards.
    pub credential: String,
    /// When it stops working.
    pub expires_at: String,
    /// The internal user it authenticates. Independent of the Telegram id, per S6.1.
    pub user_id: Uuid,
}

/// The longest assertion this route will read before deciding anything.
///
/// The payload is six short members and an Ed25519 signature is 64 bytes, so a legitimate token is
/// well under a kilobyte. The bound exists because the route is unauthenticated and the value is
/// attacker-chosen.
const MAX_ASSERTION: usize = 4096;

/// `POST /v2/sessions/telegram`.
///
/// Verify, resolve the person, mint. The resolution and the minting are ONE transaction with the
/// nonce record, so a crash between them cannot leave a session minted with its nonce unrecorded —
/// which would make the assertion replayable exactly once more.
pub async fn exchange_telegram(
    State(state): State<Arc<ApiState>>,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    // Raw bytes rather than `Json<T>`, and deliberately: an extractor rejects a request BEFORE the
    // handler runs, so its status is one nobody here chose — 415 for a missing content type, 422
    // for a body that parses but does not fit. Neither is a failure this repository names, and a
    // status it does not name reaches a client as an internal error. A handler names its own
    // failure; `captures.rs` takes bytes for a different reason and arrives at the same shape.
    body: axum::body::Bytes,
) -> Response {
    let correlation = crate::correlation_of(context);

    let Ok(body) = serde_json::from_slice::<ExchangeAssertion>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };

    // No key, no exchange. `GET /v2/capabilities` reports `telegram.mini_app` as unavailable in this
    // deployment, so a client can tell before it tries — which is what that endpoint is for.
    let Some(key) = state.assertion_key.as_ref() else {
        tracing::warn!(
            "an assertion was presented but no verification key is configured; the route is refusing"
        );
        return platform_http::reject(FailureKind::Unauthenticated);
    };

    if body.assertion.len() > MAX_ASSERTION {
        return refuse(&state, &correlation, "oversized").await;
    }

    let now = jiff::Timestamp::now();
    let claims = match assertion::verify(&body.assertion, key, &state.audience, now) {
        Ok(claims) => claims,
        Err(rejected) => {
            // The reason is for this log line and never for the caller: the difference between
            // "expired" and "bad signature" is a fact about our verification they must not probe.
            tracing::info!(%rejected, "an identity assertion was refused");
            return refuse(&state, &correlation, "rejected").await;
        }
    };

    match mint(&state, &claims, &correlation, now).await {
        Minted::Session(response) => response,
        Minted::Replayed => {
            tracing::warn!(issuer = %claims.issuer, "an identity assertion was presented twice");
            refuse(&state, &correlation, "replayed").await
        }
        Minted::Failed => platform_http::reject(FailureKind::RequestTimeout),
    }
}

/// What the transaction produced.
enum Minted {
    /// The session, ready to send.
    Session(Response),
    /// The nonce was already recorded. A replay, and the second presentation mints nothing.
    Replayed,
    /// The database refused. Nothing was written.
    Failed,
}

/// Resolve the person, record the nonce, and open the session — all or nothing.
///
/// Split from the handler along the boundary that means something: everything before it decides
/// whether the assertion is believable, and this acts on it.
async fn mint(
    state: &ApiState,
    claims: &assertion::AssertionClaims,
    correlation: &str,
    now: jiff::Timestamp,
) -> Minted {
    let Ok(mut transaction) = state.database.pool().begin().await else {
        tracing::error!("a transaction could not be started");
        return Minted::Failed;
    };

    let Some(user_id) = resolve_person(&mut transaction, &claims.subject, now).await else {
        return Minted::Failed;
    };

    // The nonce, in the same transaction as everything else. `Ok(None)` is a replay.
    match assertion::redeem(&mut *transaction, claims, user_id, now).await {
        Ok(Some(_)) => {}
        Ok(None) => return Minted::Replayed,
        Err(error) => {
            tracing::error!(%error, "the assertion could not be redeemed");
            return Minted::Failed;
        }
    }

    let Ok((credential, digest)) = session::mint_credential() else {
        tracing::error!("a session credential could not be minted");
        return Minted::Failed;
    };
    let expires_at = now + SESSION_LIFETIME;
    let opened = session::create_session(
        &mut *transaction,
        &NewSession {
            user_id,
            kind: SessionKind::TelegramMiniApp,
            device_id: None,
            audience: &state.audience,
            token: Some(digest),
            issued_at: now,
            expires_at,
        },
    )
    .await;
    let opened = match opened {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(%error, "the session could not be opened");
            return Minted::Failed;
        }
    };

    if let Err(error) = audit::record(
        &mut *transaction,
        &AuditEvent {
            audit_event_id: Uuid::now_v7(),
            actor_user_id: Some(user_id),
            actor_session_id: Some(opened.session_id),
            action: "session.exchange_assertion",
            target_kind: "session",
            target_id: Some(opened.session_id),
            outcome: AuditOutcome::Allowed,
            correlation_id: correlation.to_owned(),
        },
        now,
    )
    .await
    {
        tracing::error!(%error, "the audit record could not be written");
        return Minted::Failed;
    }

    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the session transaction could not be committed");
        return Minted::Failed;
    }

    Minted::Session(
        (
            http::StatusCode::CREATED,
            Json(SessionMinted {
                credential,
                expires_at: expires_at.to_string(),
                user_id,
            }),
        )
            .into_response(),
    )
}

/// The internal user this Telegram account belongs to, creating one on first sight.
///
/// Split from [`mint`] to keep it inside the workspace's function-length lint, along a boundary that
/// means something: this answers "who is this", and everything after it acts on the answer.
///
/// `link_external_identity` is idempotent on `(provider, external_id)` and returns the STORED user,
/// so two first-time exchanges racing each other converge on one person rather than making two.
async fn resolve_person(
    transaction: &mut sqlx::PgTransaction<'_>,
    subject: &str,
    now: jiff::Timestamp,
) -> Option<Uuid> {
    let existing = user::find_user_by_external_identity(
        &mut **transaction,
        IdentityProvider::Telegram,
        subject,
    )
    .await;
    let user_id = match existing {
        Ok(Some(user_id)) => user_id,
        Ok(None) => match user::create_user(&mut **transaction, now).await {
            Ok(created) => created.user_id,
            Err(error) => {
                tracing::error!(%error, "a user could not be created");
                return None;
            }
        },
        Err(error) => {
            tracing::error!(%error, "the external identity could not be resolved");
            return None;
        }
    };

    match user::link_external_identity(
        &mut **transaction,
        user_id,
        IdentityProvider::Telegram,
        subject,
        now,
    )
    .await
    {
        Ok(linked) => Some(linked.user_id),
        Err(error) => {
            tracing::error!(%error, "the external identity could not be linked");
            None
        }
    }
}

/// One refusal for every way of not being believed, with the reason recorded where an operator can
/// read it and nowhere a caller can.
///
/// The audit record is written on its own connection rather than in the caller's transaction,
/// because there is no caller transaction: nothing else happened. A denial with no trace is the case
/// `identity.audit_events` exists for.
async fn refuse(state: &ApiState, correlation: &str, reason: &'static str) -> Response {
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: None,
        actor_session_id: None,
        action: "session.exchange_assertion",
        target_kind: "assertion",
        target_id: None,
        outcome: AuditOutcome::Denied,
        correlation_id: correlation.to_owned(),
    };
    if let Err(error) = audit::record(state.database.pool(), &event, jiff::Timestamp::now()).await {
        tracing::error!(%error, %reason, "a denial could not be audited");
    }
    platform_http::reject(FailureKind::Unauthenticated)
}

/// How this route is described in the generated `OpenAPI` document.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v2/sessions/telegram",
    operation_id: "exchangeTelegramAssertion",
    summary: "Exchange a Telegram identity assertion for a session",
    description: "\
Takes a short-lived assertion issued by `ratatoskr-telegram` — which is the service that owns the bot \
token and validates the raw Mini App `initData` — and returns a Platform session credential.\n\n\
The credential is returned ONCE. Only its digest is stored, so it cannot be recovered afterwards; a \
client that loses it exchanges a new assertion.\n\n\
An assertion is single-use. Presenting the same one twice mints one session, and the second attempt \
is refused exactly as an invalid one is. Every refusal is the same 401: a missing key, a bad \
signature, a wrong audience, an expired assertion and a replayed one are indistinguishable from \
outside, because the difference is a fact about the verification and not about the caller.\n\n\
The internal user is independent of the Telegram account. Signing in for the first time creates one.",
    tag: "sessions",
    security: Security::None,
    parameters: &[],
    request: Some(Payload::Json("ExchangeAssertion")),
    responses: &[
        ResponseDoc {
            status: 201,
            description: "The session. Its credential appears in this response and nowhere else.",
            payload: Some(Payload::Json("SessionMinted")),
        },
        ResponseDoc {
            status: 400,
            description: "The body is not readable.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The assertion was not believed, for a reason this API does not disclose.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
