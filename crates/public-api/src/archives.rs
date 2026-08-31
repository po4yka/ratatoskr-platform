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
use ratatoskr_blob_transfer_contracts::{
    DigestHex, UploadChunkReceipt, UploadCompletionOutcome, UploadFinalizeRequest, UploadPlan,
    UploadResumptionToken, UploadSessionOpened, UploadSessionRequest, UploadSessionState,
    UploadStatusResponse,
};
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use platform_identity::audit::{self, AuditEvent, AuditOutcome};

use crate::{ApiState, Principal};

const PREPARE_ROUTE: &str = "/v1/ai-archives/{provider}";
const OPEN_ROUTE: &str = "/v1/ai-archives/{provider}/{operation_id}/uploads";
const CHUNK_ROUTE: &str =
    "/v1/ai-archives/{provider}/{operation_id}/uploads/{token}/chunks/{index}";
const STATUS_ROUTE: &str = "/v1/ai-archives/{provider}/{operation_id}/uploads/{token}/status";
const FINALIZE_ROUTE: &str = "/v1/ai-archives/{provider}/{operation_id}/uploads/{token}/finalize";
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
        || !state.health.archive_provider_ready(&provider)
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
#[expect(
    clippy::too_many_lines,
    reason = "the single transaction keeps reservation, operation, binding and audit atomic"
)]
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
        device_id: principal.device_id.unwrap_or_default(),
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

#[derive(Debug)]
struct TransferBinding {
    token: UploadResumptionToken,
    declared_size_bytes: u64,
    chunk_size_bytes: u32,
    expected_chunks: u32,
    digest_sha256: String,
    media_type: String,
    finalized: bool,
}

/// `POST /v1/ai-archives/{provider}/{operation_id}/uploads`.
#[expect(
    clippy::too_many_lines,
    reason = "the session-open transaction validates one immutable transfer declaration"
)]
pub async fn open_transfer(
    State(state): State<Arc<ApiState>>,
    Path((provider, operation_id)): Path<(String, Uuid)>,
    principal: Principal,
    body: Bytes,
) -> Response {
    if !is_supported_provider(&provider) || !is_export_agent(&state, principal).await {
        return platform_http::reject(FailureKind::NotFound);
    }
    let request: UploadSessionRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return platform_http::reject(FailureKind::InvalidRequest),
    };
    let acceptance =
        match platform_operations::find_ai_archive_acceptance(state.database.pool(), operation_id)
            .await
        {
            Ok(Some(value))
                if value.owner_user_id == principal.user_id
                    && Some(value.device_id) == principal.device_id
                    && value.provider == provider
                    && u64::try_from(value.byte_size).ok() == Some(request.declared_size_bytes)
                    && value.sha256 == request.digest.hex.as_str() =>
            {
                value
            }
            Ok(_) => return platform_http::reject(FailureKind::NotFound),
            Err(error) => {
                tracing::error!(%error, "archive transfer binding could not be read");
                return platform_http::reject(FailureKind::RequestTimeout);
            }
        };
    let Ok(plan) = UploadPlan::new(request.declared_size_bytes, request.chunk_size_bytes) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    let now = jiff::Timestamp::now();
    let existing = sqlx::query(
        "select resumption_token, declared_size_bytes, media_type, digest_sha256,
                chunk_size_bytes, expires_at
           from operations.ai_archive_transfers where operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(state.database.pool())
    .await;
    let existing = match existing {
        Ok(existing) => existing,
        Err(error) => {
            tracing::error!(%error, "archive transfer replay could not be checked");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    if let Some(row) = existing {
        let matches = row.try_get::<i64, _>("declared_size_bytes").ok()
            == i64::try_from(request.declared_size_bytes).ok()
            && row.try_get::<String, _>("media_type").ok().as_deref()
                == Some(request.media_type.as_str())
            && row.try_get::<String, _>("digest_sha256").ok().as_deref()
                == Some(request.digest.hex.as_str())
            && row.try_get::<i32, _>("chunk_size_bytes").ok()
                == i32::try_from(request.chunk_size_bytes).ok();
        if !matches {
            return platform_http::reject(FailureKind::IdempotencyConflict);
        }
        let expires = row
            .try_get::<time::OffsetDateTime, _>("expires_at")
            .ok()
            .map(from_offset);
        if let Some(expires_at) = expires.filter(|expires_at| *expires_at > now) {
            let token = row
                .try_get::<String, _>("resumption_token")
                .ok()
                .and_then(|value| UploadResumptionToken::parse(&value).ok());
            if let Some(token) = token {
                return opened(token, request.chunk_size_bytes, expires_at);
            }
        }
        let old_token = row.try_get::<String, _>("resumption_token").ok();
        if sqlx::query("delete from operations.ai_archive_transfers where operation_id = $1")
            .bind(operation_id)
            .execute(state.database.pool())
            .await
            .is_err()
        {
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        if let Some(old_token) = old_token {
            let _ = tokio::fs::remove_dir_all(state.archive_staging_root.join(old_token)).await;
        }
    }
    let token_value = format!("rst_{}", Uuid::now_v7().simple());
    let Ok(token) = UploadResumptionToken::parse(&token_value) else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let expires_at = now + jiff::SignedDuration::from_hours(24);
    let result = sqlx::query(
        "insert into operations.ai_archive_transfers
             (resumption_token, operation_id, declared_size_bytes, media_type, digest_sha256,
              chunk_size_bytes, expected_chunks, expires_at, opened_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(token.as_str())
    .bind(acceptance.operation_id)
    .bind(i64::try_from(request.declared_size_bytes).unwrap_or(i64::MAX))
    .bind(request.media_type.as_str())
    .bind(request.digest.hex.as_str())
    .bind(i32::try_from(request.chunk_size_bytes).unwrap_or(i32::MAX))
    .bind(i32::try_from(plan.expected_chunk_count()).unwrap_or(i32::MAX))
    .bind(to_offset(expires_at))
    .bind(to_offset(now))
    .execute(state.database.pool())
    .await;
    if let Err(error) = result {
        tracing::error!(%error, "archive transfer could not be opened");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    let directory = state.archive_staging_root.join(token.as_str());
    if let Err(error) = tokio::fs::create_dir_all(directory).await {
        tracing::error!(%error, "archive staging directory could not be created");
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    opened(token, request.chunk_size_bytes, expires_at)
}

fn opened(
    token: UploadResumptionToken,
    chunk_size_bytes: u32,
    expires_at: jiff::Timestamp,
) -> Response {
    (
        http::StatusCode::CREATED,
        Json(UploadSessionOpened {
            resumption_token: token,
            chunk_size_bytes,
            expires_at: ratatoskr_identifiers::WireTimestamp::from_jiff(expires_at),
            extensions: ratatoskr_identifiers::Extensions::new(),
        }),
    )
        .into_response()
}

/// `PUT /v1/ai-archives/{provider}/{operation_id}/uploads/{token}/chunks/{index}`.
pub async fn put_chunk(
    State(state): State<Arc<ApiState>>,
    Path((provider, operation_id, token_value, index)): Path<(String, Uuid, String, u32)>,
    principal: Principal,
    body: Bytes,
) -> Response {
    let Some(binding) =
        transfer_binding(&state, principal, &provider, operation_id, &token_value).await
    else {
        return platform_http::reject(FailureKind::NotFound);
    };
    if binding.finalized {
        return platform_http::reject(FailureKind::NotFound);
    }
    let Ok(plan) = UploadPlan::new(binding.declared_size_bytes, binding.chunk_size_bytes) else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let Some(expected) = plan.chunk_len(index) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    if usize::try_from(expected).ok() != Some(body.len()) {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    let digest = ratatoskr_blob_transfer_contracts::chunk_digest_hex(&body);
    let existing = sqlx::query(
        "select sha256, byte_size from operations.ai_archive_transfer_chunks
          where resumption_token = $1 and chunk_index = $2",
    )
    .bind(binding.token.as_str())
    .bind(i32::try_from(index).unwrap_or(i32::MAX))
    .fetch_optional(state.database.pool())
    .await;
    match existing {
        Ok(Some(row)) => {
            let identical = row.try_get::<String, _>("sha256").ok().as_deref()
                == Some(digest.as_str())
                && row.try_get::<i32, _>("byte_size").ok() == i32::try_from(body.len()).ok();
            if !identical {
                return platform_http::reject(FailureKind::IdempotencyConflict);
            }
            let count = transfer_chunk_count(&state, &binding.token).await;
            return Json(UploadChunkReceipt {
                resumption_token: binding.token,
                chunk_index: index,
                received_chunks_count: count,
                idempotent_replay: true,
                extensions: ratatoskr_identifiers::Extensions::new(),
            })
            .into_response();
        }
        Ok(None) => {}
        Err(_) => return platform_http::reject(FailureKind::RequestTimeout),
    }
    let directory = state.archive_staging_root.join(binding.token.as_str());
    if tokio::fs::create_dir_all(&directory).await.is_err() {
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    let temporary = directory.join(format!("{index}.{}.tmp", Uuid::now_v7().simple()));
    let published = directory.join(format!("{index}.chunk"));
    let Ok(mut file) = tokio::fs::File::create(&temporary).await else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    if file.write_all(&body).await.is_err() || file.sync_all().await.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    drop(file);
    if tokio::fs::rename(&temporary, &published).await.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    let result = sqlx::query(
        "insert into operations.ai_archive_transfer_chunks
             (resumption_token, chunk_index, sha256, byte_size, received_at)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(binding.token.as_str())
    .bind(i32::try_from(index).unwrap_or(i32::MAX))
    .bind(digest.as_str())
    .bind(i32::try_from(body.len()).unwrap_or(i32::MAX))
    .bind(to_offset(jiff::Timestamp::now()))
    .execute(state.database.pool())
    .await;
    if result.is_err() {
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    let count = transfer_chunk_count(&state, &binding.token).await;
    Json(UploadChunkReceipt {
        resumption_token: binding.token,
        chunk_index: index,
        received_chunks_count: count,
        idempotent_replay: false,
        extensions: ratatoskr_identifiers::Extensions::new(),
    })
    .into_response()
}

async fn transfer_chunk_count(state: &ApiState, token: &UploadResumptionToken) -> u32 {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from operations.ai_archive_transfer_chunks where resumption_token = $1",
    )
    .bind(token.as_str())
    .fetch_one(state.database.pool())
    .await
    .unwrap_or_default();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// `GET /v1/ai-archives/{provider}/{operation_id}/uploads/{token}/status`.
pub async fn transfer_status(
    State(state): State<Arc<ApiState>>,
    Path((provider, operation_id, token_value)): Path<(String, Uuid, String)>,
    principal: Principal,
) -> Response {
    let Some(binding) =
        transfer_binding(&state, principal, &provider, operation_id, &token_value).await
    else {
        return platform_http::reject(FailureKind::NotFound);
    };
    let Ok(rows) = sqlx::query(
        "select chunk_index from operations.ai_archive_transfer_chunks
         where resumption_token = $1 order by chunk_index",
    )
    .bind(binding.token.as_str())
    .fetch_all(state.database.pool())
    .await
    else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let received_chunks: Vec<u32> = rows
        .iter()
        .filter_map(|row| row.try_get::<i32, _>("chunk_index").ok())
        .filter_map(|index| u32::try_from(index).ok())
        .collect();
    let received_chunks_count = u32::try_from(received_chunks.len()).unwrap_or(u32::MAX);
    Json(UploadStatusResponse {
        resumption_token: binding.token,
        session_state: if binding.finalized {
            UploadSessionState::Finalized
        } else {
            UploadSessionState::Open
        },
        received_chunks,
        received_chunks_count,
        missing_chunks_count: binding
            .expected_chunks
            .saturating_sub(received_chunks_count),
        extensions: ratatoskr_identifiers::Extensions::new(),
    })
    .into_response()
}

/// `POST /v1/ai-archives/{provider}/{operation_id}/uploads/{token}/finalize`.
#[expect(
    clippy::too_many_lines,
    reason = "ordered verification and fixed-route delivery form one security boundary"
)]
pub async fn finalize_transfer(
    State(state): State<Arc<ApiState>>,
    Path((provider, operation_id, token_value)): Path<(String, Uuid, String)>,
    principal: Principal,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: Bytes,
) -> Response {
    let finalize = match serde_json::from_slice::<UploadFinalizeRequest>(&body) {
        Ok(request) if request.resumption_token.as_str() == token_value => request,
        _ => return platform_http::reject(FailureKind::InvalidRequest),
    };
    let Some(binding) =
        transfer_binding(&state, principal, &provider, operation_id, &token_value).await
    else {
        return platform_http::reject(FailureKind::NotFound);
    };
    if binding.finalized {
        return stored_completion(&binding, &provider);
    }
    let rows = match sqlx::query(
        "select chunk_index, sha256, byte_size
           from operations.ai_archive_transfer_chunks
          where resumption_token = $1 order by chunk_index",
    )
    .bind(binding.token.as_str())
    .fetch_all(state.database.pool())
    .await
    {
        Ok(rows)
            if rows.len() == usize::try_from(binding.expected_chunks).unwrap_or(usize::MAX) =>
        {
            rows
        }
        Ok(_) => return platform_http::reject(FailureKind::InvalidRequest),
        Err(_) => return platform_http::reject(FailureKind::RequestTimeout),
    };
    let directory = state.archive_staging_root.join(binding.token.as_str());
    let assembling = directory.join("archive.assembling");
    let assembled = directory.join("archive.verified");
    let Ok(mut output) = tokio::fs::File::create(&assembling).await else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let mut hasher = Sha256::new();
    let mut assembled_size = 0_u64;
    for (expected_index, row) in rows.iter().enumerate() {
        let index = match row.try_get::<i32, _>("chunk_index") {
            Ok(index) if usize::try_from(index).ok() == Some(expected_index) => index,
            _ => return platform_http::reject(FailureKind::RequestTimeout),
        };
        let Ok(chunk) = tokio::fs::read(directory.join(format!("{index}.chunk"))).await else {
            return platform_http::reject(FailureKind::RequestTimeout);
        };
        let recorded_digest = row.try_get::<String, _>("sha256").ok();
        let recorded_size = row.try_get::<i32, _>("byte_size").ok();
        if recorded_digest.as_deref()
            != Some(ratatoskr_blob_transfer_contracts::chunk_digest_hex(&chunk).as_str())
            || recorded_size != i32::try_from(chunk.len()).ok()
        {
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        hasher.update(&chunk);
        assembled_size =
            assembled_size.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if output.write_all(&chunk).await.is_err() {
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    }
    if output.sync_all().await.is_err() {
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    drop(output);
    let computed = hex_encode(&hasher.finalize());
    if computed != binding.digest_sha256 || assembled_size != binding.declared_size_bytes {
        let _ = tokio::fs::remove_file(&assembling).await;
        let _ = sqlx::query(
            "update operations.ai_archive_transfers set session_state = 'failed'
              where resumption_token = $1 and session_state = 'open'",
        )
        .bind(binding.token.as_str())
        .execute(state.database.pool())
        .await;
        let Ok(declared) = DigestHex::parse(&binding.digest_sha256) else {
            return platform_http::reject(FailureKind::RequestTimeout);
        };
        let Ok(computed) = DigestHex::parse(&computed) else {
            return platform_http::reject(FailureKind::RequestTimeout);
        };
        return Json(UploadCompletionOutcome::DigestMismatch {
            declared_sha256_hex: declared,
            computed_sha256_hex: computed,
            extensions: ratatoskr_identifiers::Extensions::new(),
        })
        .into_response();
    }
    if tokio::fs::rename(&assembling, &assembled).await.is_err() {
        return platform_http::reject(FailureKind::RequestTimeout);
    }
    let Ok(verified) = tokio::fs::read(&assembled).await else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let Ok(request) = Request::builder()
        .method("PUT")
        .body(axum::body::Body::from(verified))
    else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let correlation = crate::correlation_of(context);
    let response = state
        .gateway
        .forward_archive_receipt(crate::gateway::ArchiveReceipt {
            provider: &provider,
            principal,
            correlation_id: &correlation,
            operation_id,
            sha256: &binding.digest_sha256,
            byte_size: i64::try_from(binding.declared_size_bytes).unwrap_or(i64::MAX),
            request,
        })
        .await;
    if response.status().is_success() {
        let updated = sqlx::query(
            "update operations.ai_archive_transfers set session_state = 'finalized'
              where resumption_token = $1 and session_state = 'open'",
        )
        .bind(finalize.resumption_token.as_str())
        .execute(state.database.pool())
        .await;
        if updated.is_err() {
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        return stored_completion(&binding, &provider);
    }
    response
}

fn stored_completion(binding: &TransferBinding, provider: &str) -> Response {
    let owner = match provider {
        "chatgpt" => "ratatoskr-chatgpt",
        "claude" => "ratatoskr-claude-archive",
        _ => return platform_http::reject(FailureKind::NotFound),
    };
    let (Ok(owner_service), Ok(hex), Ok(media_type)) = (
        ratatoskr_identifiers::BlobOwner::parse(owner),
        DigestHex::parse(&binding.digest_sha256),
        ratatoskr_identifiers::MediaType::parse(&binding.media_type),
    ) else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    Json(UploadCompletionOutcome::Stored {
        blob_ref: ratatoskr_identifiers::BlobRef {
            owner_service,
            digest: ratatoskr_identifiers::ContentDigest {
                algorithm: ratatoskr_identifiers::DigestAlgorithm::Sha256,
                hex,
            },
            media_type,
            length_bytes: binding.declared_size_bytes,
        },
        extensions: ratatoskr_identifiers::Extensions::new(),
    })
    .into_response()
}

async fn transfer_binding(
    state: &ApiState,
    principal: Principal,
    provider: &str,
    operation_id: Uuid,
    token_value: &str,
) -> Option<TransferBinding> {
    if !is_supported_provider(provider) || !is_export_agent(state, principal).await {
        return None;
    }
    let token = UploadResumptionToken::parse(token_value).ok()?;
    let row = sqlx::query(
        "select t.declared_size_bytes, t.chunk_size_bytes, t.expected_chunks, t.digest_sha256,
                t.media_type, t.session_state
           from operations.ai_archive_transfers t
           join operations.ai_archive_acceptances a using (operation_id)
          where t.resumption_token = $1 and t.operation_id = $2
            and (t.session_state = 'finalized'
                 or (t.session_state = 'open' and t.expires_at > now()))
            and a.owner_user_id = $3 and a.device_id = $4 and a.provider = $5",
    )
    .bind(token.as_str())
    .bind(operation_id)
    .bind(principal.user_id)
    .bind(principal.device_id?)
    .bind(provider)
    .fetch_optional(state.database.pool())
    .await
    .ok()??;
    Some(TransferBinding {
        token,
        declared_size_bytes: u64::try_from(row.try_get::<i64, _>("declared_size_bytes").ok()?)
            .ok()?,
        chunk_size_bytes: u32::try_from(row.try_get::<i32, _>("chunk_size_bytes").ok()?).ok()?,
        expected_chunks: u32::try_from(row.try_get::<i32, _>("expected_chunks").ok()?).ok()?,
        digest_sha256: row.try_get("digest_sha256").ok()?,
        media_type: row.try_get("media_type").ok()?,
        finalized: row.try_get::<String, _>("session_state").ok()? == "finalized",
    })
}

fn to_offset(value: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

fn from_offset(value: time::OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::from_nanosecond(value.unix_timestamp_nanos())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use core::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
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
            upload_path: format!("/v1/ai-archives/{provider}/{operation_id}/uploads"),
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

/// Generated `OpenAPI` description for opening an operation-owned transfer.
pub const OPEN_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: OPEN_ROUTE,
    operation_id: "openAiArchiveTransfer",
    summary: "Open an AI archive transfer",
    description: "Opens bounded resumable staging under the prepared provider and operation.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
    ],
    request: Some(Payload::Json("UploadSessionRequest")),
    responses: &[
        ResponseDoc {
            status: 201,
            description: "The resumable session is durable.",
            payload: Some(Payload::Json("UploadSessionOpened")),
        },
        ResponseDoc {
            status: 400,
            description: "The declaration is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The credential does not authenticate.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound operation is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for one transfer chunk.
pub const CHUNK_DOC: RouteDoc = RouteDoc {
    method: Method::Put,
    path: CHUNK_ROUTE,
    operation_id: "putAiArchiveTransferChunk",
    summary: "Store one AI archive chunk",
    description: "Stores one exact indexed chunk beneath the operation-owned session.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
        Parameter {
            name: "token",
            location: In::Path,
            required: true,
            format: None,
            description: "The opaque resumption token.",
        },
        Parameter {
            name: "index",
            location: In::Path,
            required: true,
            format: None,
            description: "The zero-based chunk index.",
        },
    ],
    request: Some(Payload::Binary),
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The chunk is acknowledged.",
            payload: Some(Payload::Json("UploadChunkReceipt")),
        },
        ResponseDoc {
            status: 400,
            description: "The chunk does not match the declared plan.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound session is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for resumable transfer status.
pub const STATUS_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: STATUS_ROUTE,
    operation_id: "readAiArchiveTransferStatus",
    summary: "Read AI archive transfer status",
    description: "Returns the exact acknowledged indices for restart-safe resume.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
        Parameter {
            name: "token",
            location: In::Path,
            required: true,
            format: None,
            description: "The opaque resumption token.",
        },
    ],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The current resumable view.",
            payload: Some(Payload::Json("UploadStatusResponse")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound session is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for transfer verification and provider delivery.
pub const FINALIZE_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: FINALIZE_ROUTE,
    operation_id: "finalizeAiArchiveTransfer",
    summary: "Finalize an AI archive transfer",
    description: "Verifies ordered staged bytes and delivers them only to the operation-bound provider receipt.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
        Parameter {
            name: "token",
            location: In::Path,
            required: true,
            format: None,
            description: "The opaque resumption token.",
        },
    ],
    request: Some(Payload::Json("UploadFinalizeRequest")),
    responses: &[
        ResponseDoc {
            status: 200,
            description: "Verification produced a truthful stored or digest-mismatch outcome.",
            payload: Some(Payload::Json("UploadCompletionOutcome")),
        },
        ResponseDoc {
            status: 400,
            description: "Chunks are incomplete or the finalize request is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound session is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
