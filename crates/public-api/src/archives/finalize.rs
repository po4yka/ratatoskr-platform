//! Verifying an uploaded archive and delivering it to its receiver.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use platform_core::FailureKind;
use ratatoskr_blob_transfer_contracts::{
    DigestHex, UploadCompletionOutcome, UploadFinalizeRequest,
};
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use uuid::Uuid;

use super::{TransferBinding, hex_encode, transfer_binding};
use crate::{ApiState, Principal};

/// The size of one frame of the forwarded body.
const FRAME_BYTES: usize = 64 * 1024;

/// The file as a stream of frames, read as the receiver consumes them.
fn frames(
    mut file: tokio::fs::File,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream::try_stream! {
        let mut buffer = vec![0_u8; FRAME_BYTES];
        loop {
            let read = file.read(&mut buffer).await?;
            let Some(frame) = buffer.get(..read).filter(|frame| !frame.is_empty()) else {
                break;
            };
            yield Bytes::copy_from_slice(frame);
        }
    }
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
    // Streamed from the verified file in frames and never read into memory whole: an archive can be
    // gigabytes, and the whole point of staging it on disk was not to hold it in the heap.
    let Ok(verified) = tokio::fs::File::open(&assembled).await else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let body = axum::body::Body::from_stream(frames(verified));
    let correlation = crate::correlation_of(context);
    let response = state
        .gateway
        .forward_archive_receipt(crate::gateway::ArchiveReceipt {
            provider: &provider,
            principal,
            correlation_id: &correlation,
            operation_id,
            sha256: &binding.digest_sha256,
            byte_size: binding.declared_size_bytes,
            body,
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
        // The receiver has the bytes, so the chunks and the assembled copy have no further use.
        // Best effort: a directory that cannot be removed is a leak to clean up, not a reason to
        // tell the client that a delivered archive was not delivered.
        if let Err(error) = tokio::fs::remove_dir_all(&directory).await {
            tracing::warn!(%error, "the staging directory of a delivered archive could not be removed");
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
