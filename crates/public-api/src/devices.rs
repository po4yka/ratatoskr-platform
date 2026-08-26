//! Device pairing and the device surface: how a new installation becomes trusted, what the owner
//! sees, and how trust ends.
//!
//! ADR-0016. Trust flows FROM a session that already has it: an authenticated session mints a
//! single-use pairing code, and the one unauthenticated step — a device presenting that code — is
//! bounded by entropy, a durable per-code attempt budget, and a process-wide rate limit. Every grant commits with its audit
//! record; every refusal is the same 401, audited where an actor exists.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_identity::DeviceKind;
use platform_identity::pairing::{self, PairRequest};
use uuid::Uuid;

use crate::auth::Principal;
use crate::{ApiState, correlation_of};
use platform_identity::audit::{self, AuditEvent, AuditOutcome};

/// How long a pairing code is acceptable. Ten minutes: long enough to carry across devices,
/// short enough that a leaked code is barely worth having.
const PAIRING_CODE_TTL: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

/// How long a paired device's first session credential lives.
const DEVICE_SESSION_TTL: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);

/// How long each refresh link lives.
const REFRESH_TTL: jiff::SignedDuration = jiff::SignedDuration::from_hours(24 * 30);

/// The longest body these routes will read before deciding anything. The pair route is
/// unauthenticated and its fields are attacker-chosen; the bound comes before any parsing.
const MAX_BODY: usize = 4096;

/// What creating a pairing code may ask for.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreatePairingCode {
    /// The kind of device expected to present this code, when the initiator wants to pin one.
    pub expected_kind: Option<String>,
    /// A free-text note about what is expected to pair, at most 120 characters.
    pub label: Option<String>,
}

/// A minted pairing code.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct PairingCodeIssued {
    /// The code. Shown once, carried to the new device, never retrievable from Platform again.
    pub code: String,
    /// When it stops being acceptable.
    pub expires_at: String,
}

/// `POST /v1/devices/pairing-codes`.
pub async fn create_pairing_code(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let correlation = correlation_of(context);
    if matches!(principal.kind, platform_identity::SessionKind::Device) {
        let event = AuditEvent {
            audit_event_id: Uuid::now_v7(),
            actor_user_id: Some(principal.user_id),
            actor_session_id: Some(principal.session_id),
            action: "device.pairing_code_create",
            target_kind: "pairing_code",
            target_id: None,
            outcome: AuditOutcome::Denied,
            correlation_id: correlation,
        };
        if let Err(error) =
            audit::record(state.database.pool(), &event, jiff::Timestamp::now()).await
        {
            tracing::error!(%error, "a child-device pairing denial could not be audited");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        return platform_http::reject(FailureKind::Unauthenticated);
    }
    let Ok(request) = serde_json::from_slice::<CreatePairingCode>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };

    let expected_kind = match request.expected_kind.as_deref() {
        None => None,
        Some(raw) => match DeviceKind::from_str_opt(raw) {
            Some(kind) => Some(kind),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };
    if request.label.as_deref().is_some_and(|label| {
        label.is_empty() || label.chars().count() > 120 || label.contains(['\n', '\r'])
    }) {
        return platform_http::reject(FailureKind::InvalidRequest);
    }

    let now = jiff::Timestamp::now();
    let Ok((code, digest)) = pairing::mint_code() else {
        tracing::error!("a pairing code could not be minted");
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let expires_at = now + PAIRING_CODE_TTL;

    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let created = pairing::create_code(
        &mut transaction,
        &pairing::NewPairingCode {
            user_id: principal.user_id,
            created_by_session_id: principal.session_id,
            expected_kind,
            label: request.label.as_deref(),
            code_digest: digest,
            now,
            expires_at,
        },
    )
    .await;
    let created = match created {
        Ok(created) => created,
        Err(error) => {
            tracing::error!(%error, "the pairing code could not be stored");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: "device.pairing_code_create",
        target_kind: "pairing_code",
        target_id: Some(created.pairing_code_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.clone(),
    };
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "the pairing grant could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the pairing-code transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    (
        http::StatusCode::CREATED,
        Json(PairingCodeIssued {
            code,
            expires_at: expires_at.to_string(),
        }),
    )
        .into_response()
}

/// What a pairing device presents.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PairDevice {
    /// The single-use code minted by an authenticated session.
    pub code: String,
    /// The kind this device claims for itself.
    pub kind: String,
    /// A name its owner chose for it, at most 120 characters.
    pub display_name: Option<String>,
}

/// What pairing grants — every secret in it appears once, here, and nowhere again.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct Paired {
    /// The registered installation.
    pub device_id: Uuid,
    /// The internal user the device now belongs to.
    pub user_id: Uuid,
    /// The device's root credential. Stored only as a digest; losing it means pairing again.
    pub device_secret: String,
    /// The first session's bearer credential.
    pub credential: String,
    /// When that credential stops working.
    pub expires_at: String,
    /// The first link of the refresh chain.
    pub refresh_token: String,
    /// When that link stops being usable.
    pub refresh_expires_at: String,
}

/// `POST /v1/devices/pair`.
///
/// The flow's one unauthenticated step. A valid code registers the device under its OWNER,
/// opens the first `device` session, issues the first refresh link, consumes the code and audits
/// the grant — one transaction, all or nothing. Every unacceptable presentation receives the same
/// 401: unknown, expired, superseded, consumed and kind-mismatched are indistinguishable from
/// outside, because the difference would be an oracle (`ARCHITECTURE.md` S15).
///
/// The route takes no `Idempotency-Key` on purpose: the code IS the retry domain, and it is
/// single-use. A response lost in transit therefore costs a re-pair, whose only residue is one
/// deletable device entry — the price of credentials that appear exactly once.
pub async fn pair(
    State(state): State<Arc<ApiState>>,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let correlation = correlation_of(context);
    if !state
        .pairing_limit
        .admit(Uuid::nil(), jiff::Timestamp::now())
    {
        return platform_http::reject(FailureKind::RateLimited);
    }

    if body.len() > MAX_BODY {
        return refuse(&state, &correlation).await;
    }
    let Ok(request) = serde_json::from_slice::<PairDevice>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    if request.code.is_empty() || request.code.len() > 128 {
        return refuse(&state, &correlation).await;
    }
    let Some(declared_kind) = DeviceKind::from_str_opt(&request.kind) else {
        // A malformed body is a client error, not a refused grant: nothing was presented.
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    if request.display_name.as_deref().is_some_and(|name| {
        name.is_empty() || name.chars().count() > 120 || name.contains(['\n', '\r'])
    }) {
        return platform_http::reject(FailureKind::InvalidRequest);
    }

    let now = jiff::Timestamp::now();
    let presented = platform_identity::SecretDigest::of(&request.code);
    let Ok((device_secret, device_digest)) = platform_identity::session::mint_credential() else {
        tracing::error!("a device secret could not be minted");
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let Ok((access, access_digest)) = platform_identity::session::mint_credential() else {
        tracing::error!("a session credential could not be minted");
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let Ok((refresh, refresh_digest)) = platform_identity::session::mint_credential() else {
        tracing::error!("a refresh token could not be minted");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let outcome = pairing::redeem(
        &mut transaction,
        &PairRequest {
            presented,
            declared_kind,
            display_name: request.display_name.as_deref(),
            device_secret: device_digest,
            access_token: access_digest,
            refresh_token: refresh_digest,
            audience: &state.audience,
            now,
            access_expires_at: now + DEVICE_SESSION_TTL,
            refresh_expires_at: now + REFRESH_TTL,
        },
    )
    .await;

    let redeemed = match outcome {
        Ok(Ok(redeemed)) => redeemed,
        Ok(Err(_refused)) => return refuse(&state, &correlation).await,
        Err(error) => {
            tracing::error!(%error, "the pairing exchange failed");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(redeemed.user_id),
        actor_session_id: None,
        action: "device.pair",
        target_kind: "device",
        target_id: Some(redeemed.device.device_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.clone(),
    };
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "the pairing could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the pairing transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    (
        http::StatusCode::CREATED,
        Json(Paired {
            device_id: redeemed.device.device_id,
            user_id: redeemed.user_id,
            device_secret,
            credential: access,
            expires_at: redeemed.session.expires_at.to_string(),
            refresh_token: refresh,
            refresh_expires_at: redeemed.refresh.expires_at.to_string(),
        }),
    )
        .into_response()
}

/// One refusal for every unacceptable code, audited where an operator can read it.
///
/// Written on its own connection: there is no caller transaction, because nothing happened.
async fn refuse(state: &ApiState, correlation: &str) -> Response {
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: None,
        actor_session_id: None,
        action: "device.pair",
        target_kind: "pairing_code",
        target_id: None,
        outcome: AuditOutcome::Denied,
        correlation_id: correlation.to_owned(),
    };
    if let Err(error) = audit::record(state.database.pool(), &event, jiff::Timestamp::now()).await {
        tracing::error!(%error, "a pairing denial could not be audited");
    }
    platform_http::reject(FailureKind::Unauthenticated)
}

/// One listed device.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct DeviceSummary {
    /// The device's identity.
    pub device_id: Uuid,
    /// Which client it is.
    pub kind: String,
    /// The owner's name for it, when there is one.
    pub display_name: Option<String>,
    /// When it was registered.
    pub created_at: String,
    /// When it last authenticated, as far as the throttled liveness touch records.
    pub last_seen_at: Option<String>,
}

/// One page of [`DeviceSummary`] rows, newest first. `next_cursor` nulls at the end of the walk.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct DeviceList {
    /// The page.
    pub devices: Vec<DeviceSummary>,
    /// Pass back verbatim for the next page; `null` after the last one.
    pub next_cursor: Option<String>,
}

/// The query string of `GET /v1/devices`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct DeviceListParams {
    /// How many rows to answer with, between 1 and 100.
    pub limit: Option<String>,
    /// The continuation cursor from the previous response.
    pub cursor: Option<String>,
}

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 100;

fn encode_cursor(created_at: jiff::Timestamp, device_id: Uuid) -> String {
    format!("{}.{}", created_at.as_microsecond(), device_id)
}

fn decode_cursor(raw: &str) -> Option<(jiff::Timestamp, Uuid)> {
    let (micros, identifier) = raw.split_once('.')?;
    let device_id = Uuid::parse_str(identifier).ok()?;
    let created_at = jiff::Timestamp::from_microsecond(micros.parse::<i64>().ok()?).ok()?;
    Some((created_at, device_id))
}

/// `GET /v1/devices`.
///
/// The caller's active devices, newest first. Revoked devices are history, not state, and never
/// appear here; deletion is how a device leaves this list.
pub async fn list_devices(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    axum::extract::Query(params): axum::extract::Query<DeviceListParams>,
) -> Response {
    let limit = match params.limit.as_deref() {
        None => DEFAULT_PAGE_SIZE,
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if (1..=MAX_PAGE_SIZE).contains(&value) => value,
            _ => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };
    let after = match params.cursor.as_deref() {
        None | Some("") => None,
        Some(raw) => match decode_cursor(raw) {
            Some(anchor) => Some(anchor),
            None => return platform_http::reject(FailureKind::InvalidRequest),
        },
    };

    let devices = platform_identity::device::list_active_devices(
        state.database.pool(),
        principal.user_id,
        after,
        limit,
    )
    .await;
    let devices = match devices {
        Ok(devices) => devices,
        Err(error) => {
            tracing::error!(%error, "the device listing could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let page_full = usize::try_from(limit).is_ok_and(|bound| devices.len() == bound);
    let next_cursor = page_full
        .then_some(devices.last())
        .flatten()
        .map(|last| encode_cursor(last.created_at, last.device_id));
    let rows = devices
        .into_iter()
        .map(|device| DeviceSummary {
            device_id: device.device_id,
            kind: device.kind.as_str().to_owned(),
            display_name: device.display_name,
            created_at: device.created_at.to_string(),
            last_seen_at: device.last_seen_at.map(|seen| seen.to_string()),
        })
        .collect();

    (
        http::StatusCode::OK,
        Json(DeviceList {
            devices: rows,
            next_cursor,
        }),
    )
        .into_response()
}

/// `DELETE /v1/devices/{device_id}`.
///
/// Revokes the caller's active device AND every live session bound to it, atomically, recording
/// why for the device and for each session. Its root credential verifies false from that instant.
/// Another user's device is the same 404 as a missing one — but a real foreign target leaves a
/// denial behind for the acting user, because probing is worth seeing.
pub async fn delete_device(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    Path(device_id): Path<Uuid>,
) -> Response {
    let correlation = correlation_of(context);
    let now = jiff::Timestamp::now();
    let pool = state.database.pool();

    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let device = platform_identity::device::find_device(&mut *transaction, device_id).await;
    let device = match device {
        Ok(Some(device)) => device,
        Ok(None) => return platform_http::reject(FailureKind::NotFound),
        Err(error) => {
            tracing::error!(%error, "the device could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    if device.user_id != principal.user_id {
        let event = AuditEvent {
            audit_event_id: Uuid::now_v7(),
            actor_user_id: Some(principal.user_id),
            actor_session_id: Some(principal.session_id),
            action: "device.revoke",
            target_kind: "device",
            target_id: Some(device_id),
            outcome: AuditOutcome::Denied,
            correlation_id: correlation.clone(),
        };
        if let Err(error) = audit::record(&mut *transaction, &event, now).await {
            tracing::error!(%error, "a foreign deletion could not be audited");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        if let Err(error) = transaction.commit().await {
            tracing::error!(%error, "the denial could not be committed");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        return platform_http::reject(FailureKind::NotFound);
    }
    if !device.is_active() {
        // Already gone: deleting twice converges on the same truth as deleting once.
        return platform_http::reject(FailureKind::NotFound);
    }

    let revoked_sessions =
        platform_identity::device::revoke_device(&mut transaction, device_id, now).await;
    let revoked_sessions = match revoked_sessions {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::error!(%error, "the device could not be revoked");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    if let Err(error) = platform_identity::record_revocation(
        &mut *transaction,
        platform_identity::RevocationSubject::Device,
        device_id,
        platform_identity::RevocationReason::UserRequest,
        Some(principal.user_id),
        now,
    )
    .await
    {
        tracing::error!(%error, "the device revocation could not be recorded");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    for session_id in &revoked_sessions {
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
            tracing::error!(%error, "a cascaded revocation could not be recorded");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    }

    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: "device.revoke",
        target_kind: "device",
        target_id: Some(device_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.clone(),
    };
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "the deletion could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "the deletion transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    http::StatusCode::NO_CONTENT.into_response()
}

/// How the four routes are described in the generated document.
pub const CREATE_CODE_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/devices/pairing-codes",
    operation_id: "createPairingCode",
    summary: "Mint a single-use pairing code for a new device",
    description: "\
Starts pairing: an authenticated session mints a short-lived, single-use code that one new device \
can present to become trusted under your account.\n\n\
The code is shown ONCE and expires quickly. Creating another code sets the previous pending one \
aside, so you never hold more than one. Optionally pin the kind of device allowed to present it.",
    tag: "devices",
    security: Security::Session,
    parameters: &[],
    request: Some(Payload::Json("CreatePairingCode")),
    responses: &[
        ResponseDoc {
            status: 201,
            description: "The code, and when it expires. It appears in this response and nowhere else.",
            payload: Some(Payload::Json("PairingCodeIssued")),
        },
        ResponseDoc {
            status: 400,
            description: "The body is not readable or names an unknown kind.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "This account has spent its request allowance.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How the pairing-exchange route is described in the generated document.
pub const PAIR_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/devices/pair",
    operation_id: "pairDevice",
    summary: "Exchange a pairing code for device credentials",
    description: "\
Completes pairing from the NEW device. Presents a live single-use code and declares what kind of \
client this installation is; the answer carries the device's root secret, its first session \
credential and the first refresh link — each returned once, never recoverable afterwards.\n\n\
Every unacceptable code receives the same refusal: unknown, expired, already used, set aside and \
kind-mismatched are deliberately indistinguishable. If the response is lost after a successful \
exchange, the code is spent; pair again with a fresh code and delete the leftover device entry.",
    tag: "devices",
    security: Security::None,
    parameters: &[],
    request: Some(Payload::Json("PairDevice")),
    responses: &[
        ResponseDoc {
            status: 201,
            description: "The device is registered and its credentials appear once, here.",
            payload: Some(Payload::Json("Paired")),
        },
        ResponseDoc {
            status: 400,
            description: "The body is not readable or names an unknown kind.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The code was not accepted, for a reason this API does not disclose.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How the device-listing route is described in the generated document.
pub const LIST_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/devices",
    operation_id: "listDevices",
    summary: "List your active devices",
    description: "\
The installations currently trusted under your account, newest first. Follow `next_cursor` for \
the rest; a device stops appearing here the moment it is deleted.",
    tag: "devices",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "limit",
            location: In::Query,
            required: false,
            format: None,
            description: "Page size, between 1 and 100. Defaults to 20.",
        },
        Parameter {
            name: "cursor",
            location: In::Query,
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
            payload: Some(Payload::Json("DeviceList")),
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

/// How the deletion route is described in the generated document.
pub const DELETE_DOC: RouteDoc = RouteDoc {
    method: Method::Delete,
    path: "/v1/devices/{device_id}",
    operation_id: "deleteDevice",
    summary: "Delete one of your devices",
    description: "\
Revokes the device and every session opened by it, together and immediately: its root secret \
stops working and none of its sessions authenticate afterwards. Another user's device and a \
nonexistent one are the same answer.",
    tag: "devices",
    security: Security::Session,
    parameters: &[Parameter {
        name: "device_id",
        location: In::Path,
        required: true,
        format: Some("uuid"),
        description: "The device to revoke.",
    }],
    request: None,
    responses: &[
        ResponseDoc {
            status: 204,
            description: "The device and all of its sessions are revoked.",
            payload: None,
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "No such device, or it belongs to somebody else — indistinguishable on purpose.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was changed.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
