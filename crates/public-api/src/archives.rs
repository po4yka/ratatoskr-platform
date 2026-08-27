//! Device-owned acceptance and delivery of provider archive bytes.
//!
//! Edge owns the operation and the immutable request binding. A provider service owns the bytes
//! once they cross its loopback receipt boundary, as well as parsing and completeness.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse as _, Response};
use http::HeaderMap;
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_idempotency::{Digest, Outcome};
use platform_identity::{DeviceKind, SessionKind};
use uuid::Uuid;

use platform_identity::audit::{self, AuditEvent, AuditOutcome};

use crate::{ApiState, Principal};

const PREPARE_ROUTE: &str = "/v1/ai-archives/{provider}";
const UPLOAD_ROUTE: &str = "/v1/ai-archives/{provider}/{operation_id}/content";
const OPERATION_KIND: &str = "ai_archive.import";
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Digest-first archive metadata. The body bytes arrive only at the operation-bound upload path.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PrepareArchive {
    /// Lowercase hexadecimal SHA-256 of the exact bytes that will be delivered.
    pub sha256: String,
    /// Exact number of bytes that will be delivered.
    pub byte_size: i64,
}

/// A durable operation plus the only upload path that may deliver its archive.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct ArchivePrepared {
    /// The operation to poll for importer processing and completeness.
    pub operation_id: Uuid,
    /// Edge has durably accepted metadata only; it has not received the archive bytes yet.
    pub status: &'static str,
    /// Relative operation-bound endpoint to which the archive bytes may be streamed with `PUT`.
    pub upload_path: String,
}

/// The exact request whose bytes the idempotency ledger fingerprints before Edge accepts it.
struct ArchivePreparation {
    provider: String,
    key: String,
    archive: PrepareArchive,
    body: Bytes,
    correlation: String,
}

/// `POST /v1/ai-archives/{provider}`.
pub async fn prepare(
    State(state): State<Arc<ApiState>>,
    Path(provider): Path<String>,
    principal: Principal,
    headers: HeaderMap,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: Bytes,
) -> Response {
    if !is_supported_provider(&provider)
        || !state.gateway.has_archive_receiver(&provider)
        || !is_export_agent(&state, principal).await
    {
        return platform_http::reject(FailureKind::NotFound);
    }
    let (key, archive) = match parse(
        &headers,
        &body,
        state.gateway.transfer_budget().max_body_bytes,
    ) {
        Ok(value) => value,
        Err(kind) => return platform_http::reject(kind),
    };
    accept(
        &state,
        principal,
        ArchivePreparation {
            provider,
            key,
            archive,
            body,
            correlation: crate::correlation_of(context),
        },
    )
    .await
}

/// Write the idempotency reservation, operation, receipt binding and audit event atomically.
async fn accept(
    state: &ApiState,
    principal: Principal,
    preparation: ArchivePreparation,
) -> Response {
    let ArchivePreparation {
        provider,
        key,
        archive,
        body,
        correlation,
    } = preparation;
    let route = format!("/v1/ai-archives/{provider}");
    let now = jiff::Timestamp::now();
    let pool = state.database.pool();
    let Ok(mut transaction) = pool.begin().await else {
        tracing::error!("archive preparation transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let reservation = match platform_idempotency::reserve(
        &mut transaction,
        principal.user_id,
        &route,
        OPERATION_KIND,
        Digest::of_key(&key),
        Digest::of_body(&body),
        now,
        state.idempotency_ttl,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "archive idempotency key could not be reserved");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let record_id = match reservation.outcome() {
        Outcome::Proceed(record_id) => record_id,
        Outcome::Replay(operation_id) => return accepted(&provider, operation_id),
        Outcome::Refuse => return platform_http::reject(FailureKind::IdempotencyConflict),
    };
    let operation = match platform_operations::accept(
        &mut *transaction,
        principal.user_id,
        OPERATION_KIND,
        &correlation,
        Some(&key),
        now,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "archive operation could not be accepted");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let acceptance = platform_operations::AiArchiveAcceptance {
        operation_id: operation.operation_id,
        owner_user_id: principal.user_id,
        provider: provider.clone(),
        sha256: archive.sha256,
        byte_size: archive.byte_size,
        accepted_at: now,
    };
    if let Err(error) =
        platform_operations::record_ai_archive_acceptance(&mut *transaction, &acceptance).await
    {
        tracing::error!(%error, "archive operation binding could not be stored");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = platform_idempotency::complete(
        &mut *transaction,
        record_id,
        Some(operation.operation_id),
        202,
        now,
    )
    .await
    {
        tracing::error!(%error, "archive idempotency record could not be completed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: "ai_archive.prepare",
        target_kind: "operation",
        target_id: Some(operation.operation_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation,
    };
    if let Err(error) = audit::record(&mut *transaction, &event, now).await {
        tracing::error!(%error, "archive preparation could not be audited");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "archive preparation transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    accepted(&provider, operation.operation_id)
}

/// `PUT /v1/ai-archives/{provider}/{operation_id}/content`.
pub async fn upload(
    State(state): State<Arc<ApiState>>,
    Path((provider, operation_id)): Path<(String, Uuid)>,
    principal: Principal,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    request: Request,
) -> Response {
    if !is_supported_provider(&provider)
        || !state.gateway.has_archive_receiver(&provider)
        || !is_export_agent(&state, principal).await
    {
        return platform_http::reject(FailureKind::NotFound);
    }
    let acceptance =
        match platform_operations::find_ai_archive_acceptance(state.database.pool(), operation_id)
            .await
        {
            Ok(Some(value))
                if value.owner_user_id == principal.user_id && value.provider == provider =>
            {
                value
            }
            Ok(_) => return platform_http::reject(FailureKind::NotFound),
            Err(error) => {
                tracing::error!(%error, "archive acceptance could not be read");
                return platform_http::reject(FailureKind::RequestTimeout);
            }
        };
    let operation = match platform_operations::find(state.database.pool(), operation_id).await {
        Ok(Some(value))
            if value.owner_user_id == principal.user_id && value.kind == OPERATION_KIND =>
        {
            value
        }
        Ok(_) => return platform_http::reject(FailureKind::NotFound),
        Err(error) => {
            tracing::error!(%error, "archive operation could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    if matches!(
        operation.status,
        ratatoskr_operation_contracts::OperationStatus::Succeeded
            | ratatoskr_operation_contracts::OperationStatus::PartiallySucceeded
            | ratatoskr_operation_contracts::OperationStatus::Failed
            | ratatoskr_operation_contracts::OperationStatus::Cancelled
    ) {
        return platform_http::reject(FailureKind::NotFound);
    }

    let correlation = crate::correlation_of(context);
    let response = state
        .gateway
        .forward_archive_receipt(crate::gateway::ArchiveReceipt {
            provider: &provider,
            principal,
            correlation_id: &correlation,
            operation_id,
            sha256: &acceptance.sha256,
            byte_size: acceptance.byte_size,
            request,
        })
        .await;
    if !response.status().is_success()
        && let Err(error) = platform_operations::fail_ai_archive_delivery(
            state.database.pool(),
            operation_id,
            jiff::Timestamp::now(),
        )
        .await
    {
        tracing::error!(%error, "archive delivery failure could not be recorded");
    }
    response
}

async fn is_export_agent(state: &ApiState, principal: Principal) -> bool {
    let Some(device_id) = principal.device_id else {
        return false;
    };
    if principal.kind != SessionKind::Device {
        return false;
    }
    matches!(
        platform_identity::device::find_device(state.database.pool(), device_id).await,
        Ok(Some(device)) if device.user_id == principal.user_id && device.is_active() && device.kind == DeviceKind::ExportAgent
    )
}

fn is_supported_provider(provider: &str) -> bool {
    matches!(provider, "chatgpt" | "claude")
}

fn parse(
    headers: &HeaderMap,
    body: &[u8],
    maximum_byte_size: u64,
) -> Result<(String, PrepareArchive), FailureKind> {
    let key = headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 255)
        .ok_or(FailureKind::MissingIdempotencyKey)?
        .to_owned();
    let archive: PrepareArchive =
        serde_json::from_slice(body).map_err(|_| FailureKind::InvalidRequest)?;
    if archive.byte_size <= 0
        || u64::try_from(archive.byte_size).map_or(true, |size| size > maximum_byte_size)
        || archive.sha256.len() != 64
        || !archive
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(FailureKind::InvalidRequest);
    }
    Ok((key, archive))
}

fn accepted(provider: &str, operation_id: Uuid) -> Response {
    (
        http::StatusCode::ACCEPTED,
        Json(ArchivePrepared {
            operation_id,
            status: "accepted",
            upload_path: format!("/v1/ai-archives/{provider}/{operation_id}/content"),
        }),
    )
        .into_response()
}

/// Generated `OpenAPI` description for preparation. The body upload is intentionally a binary
/// transport endpoint and uses the operation-bound URL returned here.
pub const PREPARE_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: PREPARE_ROUTE,
    operation_id: "prepareAiArchive",
    summary: "Prepare an AI archive import",
    description: "Accepts immutable metadata for a registered export-agent archive and returns the operation-bound upload path. It does not mean that archive bytes have been received or imported.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "`chatgpt` or `claude`.",
        },
        Parameter {
            name: "Idempotency-Key",
            location: In::Header,
            required: true,
            format: None,
            description: "A client-chosen 1 to 255 character replay key.",
        },
    ],
    request: Some(Payload::Json("PrepareArchive")),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "Metadata accepted durably; upload bytes to the returned path.",
            payload: Some(Payload::Json("ArchivePrepared")),
        },
        ResponseDoc {
            status: 400,
            description: "The metadata or idempotency header is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The provider is unsupported or this is not a registered export-agent device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The idempotency key is in use for different metadata.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for operation-bound binary delivery.
pub const UPLOAD_DOC: RouteDoc = RouteDoc {
    method: Method::Put,
    path: UPLOAD_ROUTE,
    operation_id: "uploadAiArchiveContent",
    summary: "Deliver prepared AI archive bytes",
    description: "Streams bytes to the configured provider importer. Edge injects the operation, digest and size claims; callers cannot set them.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The provider selected during preparation.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation identifier.",
        },
    ],
    request: Some(Payload::Binary),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "The importer accepted the streamed archive for asynchronous processing.",
            payload: None,
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The operation is not a deliverable archive owned by this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 503,
            description: "The configured importer could not be reached; the operation is marked failed with a safe retryable diagnostic.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "The configured importer did not respond within its bounded receipt budget.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
