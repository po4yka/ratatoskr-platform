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

use crate::{ApiState, correlation_of};

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

/// `POST /v1/sessions/telegram`.
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

    // No key, no exchange. `GET /v1/capabilities` reports `telegram.mini_app` as unavailable in this
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
    path: "/v1/sessions/telegram",
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

// -------------------------------------------------------------------------------------------------
// The lifecycle surface: seeing your sessions, and ending them.
//
// ADR-0016. Listing is how the settings page answers "where am I signed in"; revocation is how it
// answers "sign that out". Both are owner-scoped with the anti-oracle rule the rest of this API
// observes: somebody else's session and a missing one are the same 404 — but a real foreign target
// leaves a denial behind, because probing is worth seeing.
// -------------------------------------------------------------------------------------------------

use axum::extract::Path;
use platform_api_doc::{In as DocIn, Parameter};

/// One listed session: what kind it is, what device carries it when one does, and its liveness.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SessionSummary {
    /// The session's identity.
    pub session_id: Uuid,
    /// How it was established.
    pub kind: String,
    /// The bound device, when the kind requires one.
    pub device: Option<SessionDeviceRef>,
    /// When it was issued.
    pub issued_at: String,
    /// When it stops being valid on its own.
    pub expires_at: String,
    /// When it last authenticated, as far as the throttled liveness touch records.
    pub last_seen_at: Option<String>,
}

/// A device reference inside [`SessionSummary`].
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SessionDeviceRef {
    /// The device's identity.
    pub device_id: Uuid,
    /// Which client it is.
    pub kind: String,
    /// The owner's name for it, when there is one.
    pub display_name: Option<String>,
}

/// One page of [`SessionSummary`] rows, newest first. `next_cursor` nulls at the end of the walk.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SessionList {
    /// The page.
    pub sessions: Vec<SessionSummary>,
    /// Pass back verbatim for the next page; `null` after the last one.
    pub next_cursor: Option<String>,
}

/// The query string of `GET /v1/sessions`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SessionListParams {
    /// How many rows to answer with, between 1 and 100.
    pub limit: Option<String>,
    /// The continuation cursor from the previous response.
    pub cursor: Option<String>,
}

const SESSIONS_PAGE_DEFAULT: i64 = 20;
const SESSIONS_PAGE_MAX: i64 = 100;

fn encode_session_cursor(issued_at: jiff::Timestamp, session_id: Uuid) -> String {
    format!("{}.{}", issued_at.as_microsecond(), session_id)
}

fn decode_session_cursor(raw: &str) -> Option<(jiff::Timestamp, Uuid)> {
    let (micros, identifier) = raw.split_once('.')?;
    let session_id = Uuid::parse_str(identifier).ok()?;
    let issued_at = jiff::Timestamp::from_microsecond(micros.parse::<i64>().ok()?).ok()?;
    Some((issued_at, session_id))
}

/// `GET /v1/sessions`.
///
/// The caller's live sessions — unrevoked and unexpired — across every kind, newest first,
/// keyset-paginated so pages never shift under concurrent sign-ins.
pub async fn list_sessions(
    State(state): State<Arc<ApiState>>,
    principal: crate::Principal,
    axum::extract::Query(params): axum::extract::Query<SessionListParams>,
) -> Response {
    let limit = match params.limit.as_deref() {
        None => SESSIONS_PAGE_DEFAULT,
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if (1..=SESSIONS_PAGE_MAX).contains(&value) => value,
            _ => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };
    let after = match params.cursor.as_deref() {
        None | Some("") => None,
        Some(raw) => match decode_session_cursor(raw) {
            Some(anchor) => Some(anchor),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };

    let sessions = platform_identity::session::list_live_sessions(
        state.database.pool(),
        principal.user_id,
        jiff::Timestamp::now(),
        after,
        limit,
    )
    .await;
    let sessions = match sessions {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::error!(%error, "the session listing could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let page_full = usize::try_from(limit).is_ok_and(|bound| sessions.len() == bound);
    let next_cursor = page_full
        .then_some(sessions.last())
        .flatten()
        .map(|last| encode_session_cursor(last.issued_at, last.session_id));
    let rows = sessions
        .into_iter()
        .map(|s| SessionSummary {
            session_id: s.session_id,
            kind: s.kind.as_str().to_owned(),
            device: s.device.map(|d| SessionDeviceRef {
                device_id: d.device_id,
                kind: d.kind.as_str().to_owned(),
                display_name: d.display_name,
            }),
            issued_at: s.issued_at.to_string(),
            expires_at: s.expires_at.to_string(),
            last_seen_at: s.last_seen_at.map(|seen| seen.to_string()),
        })
        .collect();

    (
        http::StatusCode::OK,
        Json(SessionList {
            sessions: rows,
            next_cursor,
        }),
    )
        .into_response()
}

/// `DELETE /v1/sessions/{session_id}`.
///
/// Revokes one of the caller's live sessions in place — auditable history stays, access ends
/// now — recording why. Repeating the call, naming a dead session, a foreign one or none at all:
/// the same 404 each time.
///
/// No `Idempotency-Key`: the identifier is the idempotency domain, exactly as cancellation's is.
pub async fn revoke_session(
    State(state): State<Arc<ApiState>>,
    principal: crate::Principal,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    Path(session_id): Path<Uuid>,
) -> Response {
    let correlation = correlation_of(context);
    let now = jiff::Timestamp::now();
    let pool = state.database.pool();

    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let subject = platform_identity::session::find_session(&mut *transaction, session_id).await;
    let subject = match subject {
        Ok(Some(session)) => session,
        Ok(None) => return platform_http::reject(FailureKind::NotFound),
        Err(error) => {
            tracing::error!(%error, "the session could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    if subject.user_id != principal.user_id {
        if let Err(error) =
            record_denial(&mut transaction, &principal, &correlation, session_id, now).await
        {
            tracing::error!(%error, "a foreign revocation could not be audited");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        if let Err(error) = transaction.commit().await {
            tracing::error!(%error, "the denial could not be committed");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        return platform_http::reject(FailureKind::NotFound);
    }
    if !subject.is_live_at(now) {
        // Already ended: the repeat converges on the same truth as a miss.
        return platform_http::reject(FailureKind::NotFound);
    }

    if let Err(error) =
        platform_identity::session::revoke_session(&mut *transaction, session_id, now).await
    {
        tracing::error!(%error, "the session could not be revoked");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = platform_identity::record_revocation(
        &mut *transaction,
        platform_identity::RevocationSubject::Session,
        session_id,
        platform_identity::RevocationReason::UserRequest,
        Some(principal.user_id),
        now,
    )
    .await
    {
        tracing::error!(%error, "the revocation could not be recorded");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: "session.revoke",
        target_kind: "session",
        target_id: Some(session_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.clone(),
    };
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "the revocation could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the revocation could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    http::StatusCode::NO_CONTENT.into_response()
}

/// What revoke-all answers.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct RevokedAll {
    /// How many live sessions ended, including the calling one.
    pub revoked: u64,
}

/// `POST /v1/sessions/revoke-all`.
///
/// Ends every live session you own — including the one making this call, which is why the answer
/// arrives before your next request does. Plain truth, no carve-out to remember. Devices are
/// untouched on purpose: killing logins must not brick installations, and a device recovers with
/// one call carrying its root secret.
pub async fn revoke_all(
    State(state): State<Arc<ApiState>>,
    principal: crate::Principal,
    context: Option<axum::Extension<platform_http::RequestContext>>,
) -> Response {
    let correlation = correlation_of(context);
    let now = jiff::Timestamp::now();
    let pool = state.database.pool();

    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    // Collect then revoke in the caller's transaction, so every revoked session gets both its
    // fast-path instant and its durable why, atomically.
    let live = platform_identity::session::list_live_session_ids(
        &mut *transaction,
        principal.user_id,
        now,
    )
    .await;
    let live = match live {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(%error, "the live sessions could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let revoked = platform_identity::session::revoke_all_sessions_of_user(
        &mut *transaction,
        principal.user_id,
        now,
    )
    .await;
    let _ = match revoked {
        Ok(revoked) => revoked,
        Err(error) => {
            tracing::error!(%error, "the sessions could not be revoked");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    for session_id in &live {
        if let Err(error) = platform_identity::record_revocation(
            &mut *transaction,
            platform_identity::RevocationSubject::Session,
            *session_id,
            platform_identity::RevocationReason::UserRequest,
            Some(principal.user_id),
            now,
        )
        .await
        {
            tracing::error!(%error, "a sweep revocation could not be recorded");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    }

    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: "session.revoke_all",
        target_kind: "session",
        target_id: None,
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.clone(),
    };
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "the sweep could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the sweep could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    (
        http::StatusCode::OK,
        Json(RevokedAll {
            revoked: u64::try_from(live.len()).unwrap_or(u64::MAX),
        }),
    )
        .into_response()
}

/// The denial an authenticated actor leaves behind when they reach for a session that is not
/// theirs. Written in the caller's transaction: the refusal and its trace commit together.
async fn record_denial(
    transaction: &mut sqlx::PgTransaction<'_>,
    principal: &crate::Principal,
    correlation: &str,
    target: Uuid,
    now: jiff::Timestamp,
) -> Result<(), platform_persistence::PersistenceError> {
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: "session.revoke",
        target_kind: "session",
        target_id: Some(target),
        outcome: AuditOutcome::Denied,
        correlation_id: correlation.to_owned(),
    };
    audit::record(&mut **transaction, &event, now).await
}

/// How the lifecycle routes are described in the generated document.
/// How the session-listing route is described in the generated document.
pub const LIST_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sessions",
    operation_id: "listSessions",
    summary: "List your active sessions",
    description: "\
Where you are signed in: every live session of yours, whichever way it was established, newest \
first, each with its kind, its bound device when it has one, and when it last saw use. Follow \
`next_cursor` for the rest.",
    tag: "sessions",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "limit",
            location: DocIn::Query,
            required: false,
            format: None,
            description: "Page size, between 1 and 100. Defaults to 20.",
        },
        Parameter {
            name: "cursor",
            location: DocIn::Query,
            required: false,
            format: None,
            description: "The `next_cursor` value of the previous response, verbatim.",
        },
    ],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "One page. `next_cursor` is null after the last one.",
            payload: Some(Payload::Json("SessionList")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How the single-revocation route is described in the generated document.
pub const REVOKE_DOC: RouteDoc = RouteDoc {
    method: Method::Delete,
    path: "/v1/sessions/{session_id}",
    operation_id: "revokeSession",
    summary: "Revoke one of your sessions",
    description: "\
Signs that session out immediately; its record remains for audit. A session belonging to \
somebody else, an already-dead one and a nonexistent identifier are all the same answer.",
    tag: "sessions",
    security: Security::Session,
    parameters: &[Parameter {
        name: "session_id",
        location: DocIn::Path,
        required: true,
        format: Some("uuid"),
        description: "The session to revoke.",
    }],
    request: None,
    responses: &[
        ResponseDoc {
            status: 204,
            description: "The session no longer authenticates anything.",
            payload: None,
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "No such live session of yours — indistinguishable from somebody else's.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was changed.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How the revoke-all route is described in the generated document.
pub const REVOKE_ALL_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sessions/revoke-all",
    operation_id: "revokeAllSessions",
    summary: "Revoke every one of your sessions",
    description: "\
Signs out everywhere at once, including the session making this call. Devices are deliberately \
untouched: a registered device recovers by presenting its root secret to the device login route.",
    tag: "sessions",
    security: Security::Session,
    parameters: &[],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "Every live session is revoked; the answer counts them.",
            payload: Some(Payload::Json("RevokedAll")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was changed.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
