//! Owner editing from a site at Edge (workspace change `add-site-owner-editing-and-live`, design
//! D6; spec `site-owner-editing`). The caller is a site owner session of the site whose effective
//! set holds the route's capability; the dispatcher has checked both.
//!
//! - The draft reads go to the sites service, which answers the source of a note in scope.
//! - The upload resolves the note's connection through the loopback scope query of the sites
//!   service, and stores the content in `ratatoskr-notes` through its transfer route: one session,
//!   one chunk, finalize. Edge keeps none of it.
//! - The edit resolves the note to a connection and a path, opens one operation and enqueues one
//!   `notes.file.write_requested.v1` with the origin `site:<site id>`, as the write route of the
//!   primary session does. The operation is recorded in `operations.site_edits`, so that its success
//!   or its conflict reaches the site's event stream.
//! - The conflict read asks notes for the conflict and the sites service for the note in scope, and
//!   answers neither the connection nor the vault path.
//!
//! The claims that Edge mints for notes name the session kind `site_owner` and the origin
//! `x-ratatoskr-origin: site:<site id>`; Edge never presents the session as primary.

use std::sync::Arc;

use axum::body::Body;
use axum::response::{IntoResponse as _, Response};
use http::{HeaderValue, StatusCode};
use platform_core::FailureKind;
use platform_http::RequestContext;
use ratatoskr_identifiers::{
    BlobRef, ContentDigest, DigestAlgorithm, DigestHex, NotesConnectionId,
};
use ratatoskr_notes_contracts::{UpdateFile, VaultWritePath, WriteOperation};
use ratatoskr_sites_contracts::SourceRef;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::gateway::{Claims, claim_headers};
use crate::intake::{Intake, Minted, SiteEditRow, admit};
use crate::site_docs::{
    SiteBlobUploaded, SiteConflict, SiteConflictTexts, SiteEditAccepted, SiteEditRequest,
    SiteMergeProposal,
};
use crate::{ApiState, Principal, notes_writes};

/// The largest content of one upload: one chunk of the transfer contract.
const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;

/// The largest edit body.
const MAX_EDIT_BYTES: usize = 16 * 1024;

/// The largest answer of a loopback question or of the conflict read.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// The ledger route of an edit: one key space per owner, apart from the write route of notes.
const EDIT_LEDGER_ROUTE: &str = "/v1/sites/edits";

/// The claim that names who asked for a notes request made for a site owner session.
const HEADER_ORIGIN: &str = "x-ratatoskr-origin";

/// The answer of `GET /internal/sites/{site}/sources/{source_ref}`.
#[derive(Debug, serde::Deserialize)]
struct Scoped {
    connection_id: Uuid,
    path: String,
}

/// The answer of `GET /internal/sites/{site}/scope?connection_id=&path=`.
#[derive(Debug, serde::Deserialize)]
struct InScope {
    source_ref: String,
}

/// The claims of a request that Edge makes for the site owner session `principal` of `site`.
fn site_claims(principal: Principal, correlation: &str, site: &str) -> http::HeaderMap {
    let mut headers = claim_headers(Claims {
        principal,
        device_kind: None,
        site_id: None,
        correlation_id: correlation,
    });
    if let Ok(origin) = HeaderValue::from_str(&format!("site:{site}")) {
        headers.insert(HEADER_ORIGIN, origin);
    }
    headers
}

/// A request of Edge to `service` at `path`, with the claims of the site owner session.
fn loopback(
    state: &ApiState,
    service: &str,
    method: http::Method,
    path: &str,
    headers: http::HeaderMap,
    body: Body,
) -> Result<hyper::Request<Body>, FailureKind> {
    let listener = state
        .gateway
        .service_listener(service)
        .ok_or(FailureKind::UpstreamUnavailable)?;
    let mut request = hyper::Request::builder()
        .method(method)
        .uri(format!("http://{listener}{path}"))
        .body(body)
        .map_err(|_| FailureKind::UpstreamUnavailable)?;
    *request.headers_mut() = headers;
    Ok(request)
}

/// The connection and the path of `source_ref` in the publications of `site`; `NotFound` for a
/// note outside them.
async fn scope_of(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
    source_ref: &str,
) -> Result<Scoped, FailureKind> {
    let source = SourceRef::parse(source_ref).map_err(|_| FailureKind::NotFound)?;
    let request = loopback(
        state,
        crate::sites::SERVICE,
        http::Method::GET,
        &format!("/internal/sites/{site}/sources/{}", source.as_str()),
        site_claims(principal, correlation, site),
        Body::empty(),
    )?;
    state
        .gateway
        .fetch_json(crate::sites::SERVICE, request, true, MAX_ANSWER_BYTES)
        .await
}

/// `POST /v1/sites/{site}/drafts/sources/{source_ref}/blobs`.
pub(crate) async fn upload(
    state: &Arc<ApiState>,
    context: &RequestContext,
    principal: Principal,
    site: &str,
    source_ref: &str,
    body: Body,
) -> Response {
    let correlation = context.correlation_id.to_string();
    match upload_content(state, principal, &correlation, site, source_ref, body).await {
        Ok(uploaded) => (StatusCode::CREATED, axum::Json(uploaded)).into_response(),
        Err(refusal) => platform_http::reject(refusal),
    }
}

async fn upload_content(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
    source_ref: &str,
    body: Body,
) -> Result<SiteBlobUploaded, FailureKind> {
    let scoped = scope_of(state, principal, correlation, site, source_ref).await?;
    // Only content over the bound is too large; a body that breaks off is an invalid request.
    let content = axum::body::to_bytes(body, MAX_CONTENT_BYTES)
        .await
        .map_err(|error| {
            // The bound of this read, or the bound of the route's layer, wrapped in a body error.
            let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
            while let Some(current) = cause {
                if current.is::<http_body_util::LengthLimitError>() {
                    return FailureKind::PayloadTooLarge;
                }
                cause = current.source();
            }
            FailureKind::InvalidRequest
        })?;
    if content.is_empty() {
        return Err(FailureKind::InvalidRequest);
    }
    let hash = format!("{:x}", Sha256::digest(&content));
    let blobs = format!("/v1/notes/connections/{}/blobs", scoped.connection_id);
    let claims = site_claims(principal, correlation, site);
    let mut json_claims = claims.clone();
    json_claims.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let mut chunk_claims = claims.clone();
    chunk_claims.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    let declared = serde_json::json!({
        "declared_size_bytes": content.len(),
        "media_type": "text/markdown",
        "digest": {"algorithm": "sha256", "hex": hash},
        "chunk_size_bytes": ratatoskr_blob_transfer_contracts::CHUNK_SIZE_MAX_BYTES,
    });
    let opened: serde_json::Value = notes_json(
        state,
        http::Method::POST,
        &blobs,
        &json_claims,
        json_body(&declared),
    )
    .await?;
    let token = opened
        .get("resumption_token")
        .and_then(serde_json::Value::as_str)
        .ok_or(FailureKind::UpstreamInvalidResponse)?
        .to_owned();
    let sent = notes_json::<serde_json::Value>(
        state,
        http::Method::PUT,
        &format!("{blobs}/{token}/chunks/0"),
        &chunk_claims,
        Body::from(content),
    )
    .await;
    let finalized = match sent {
        Ok(_) => {
            notes_json::<serde_json::Value>(
                state,
                http::Method::POST,
                &format!("{blobs}/{token}/finalize"),
                &json_claims,
                json_body(&serde_json::json!({"resumption_token": token})),
            )
            .await
        }
        Err(refusal) => Err(refusal),
    };
    let finalized = match finalized {
        Ok(finalized) => finalized,
        Err(refusal) => {
            close_session(state, &format!("{blobs}/{token}"), &claims).await;
            return Err(refusal);
        }
    };
    if finalized.get("outcome").and_then(serde_json::Value::as_str) != Some("stored") {
        tracing::warn!("notes did not store the content of a site edit");
        return Err(FailureKind::UpstreamInvalidResponse);
    }
    let content_blob: BlobRef = finalized
        .get("blob_ref")
        .cloned()
        .and_then(|blob| serde_json::from_value(blob).ok())
        .ok_or(FailureKind::UpstreamInvalidResponse)?;
    Ok(SiteBlobUploaded {
        content_blob,
        content_hash: hash,
    })
}

/// Close the notes upload session at `session` after a failed step (review L9), so that it does
/// not hold one of the connection's sessions until it expires. Best effort: notes answers 204, or
/// 404 `blob_transfer.session_unknown` for a session it no longer has.
async fn close_session(state: &ApiState, session: &str, claims: &http::HeaderMap) {
    let Ok(request) = loopback(
        state,
        "notes",
        http::Method::DELETE,
        session,
        claims.clone(),
        Body::empty(),
    ) else {
        return;
    };
    match state.gateway.send_control("notes", request).await {
        Ok(status) if status.is_success() || status == StatusCode::NOT_FOUND => {}
        Ok(_) | Err(_) => tracing::warn!("a notes upload session could not be closed"),
    }
}

fn json_body(value: &serde_json::Value) -> Body {
    Body::from(value.to_string())
}

async fn notes_json<T: serde::de::DeserializeOwned>(
    state: &ApiState,
    method: http::Method,
    path: &str,
    claims: &http::HeaderMap,
    body: Body,
) -> Result<T, FailureKind> {
    let request = loopback(state, "notes", method, path, claims.clone(), body)?;
    state
        .gateway
        .fetch_json("notes", request, true, MAX_ANSWER_BYTES)
        .await
}

/// `POST /v1/sites/{site}/edits`.
pub(crate) async fn edit(
    state: &Arc<ApiState>,
    context: &RequestContext,
    principal: Principal,
    site: &str,
    body: Body,
) -> Response {
    let correlation = context.correlation_id.to_string();
    match accept_edit(state, principal, &correlation, site, body).await {
        Ok(operation_id) => (
            StatusCode::ACCEPTED,
            axum::Json(SiteEditAccepted {
                operation_id,
                status: "accepted".to_owned(),
            }),
        )
            .into_response(),
        Err(refusal) => platform_http::reject(refusal),
    }
}

/// The checked parts of an edit body.
struct Checked {
    request: SiteEditRequest,
    base_hash: DigestHex,
}

fn check(bytes: &[u8]) -> Result<Checked, FailureKind> {
    let request: SiteEditRequest =
        serde_json::from_slice(bytes).map_err(|_| FailureKind::InvalidRequest)?;
    let base_hash =
        DigestHex::parse(&request.base_hash).map_err(|_| FailureKind::InvalidRequest)?;
    let content_hash =
        DigestHex::parse(&request.content_hash).map_err(|_| FailureKind::InvalidRequest)?;
    let key_valid = !request.idempotency_key.is_empty() && request.idempotency_key.len() <= 255;
    let blob_valid = request.content_blob.digest
        == ContentDigest {
            algorithm: DigestAlgorithm::Sha256,
            hex: content_hash,
        };
    if !key_valid || !blob_valid || SourceRef::parse(&request.source_ref).is_err() {
        return Err(FailureKind::InvalidRequest);
    }
    Ok(Checked { request, base_hash })
}

async fn accept_edit(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
    body: Body,
) -> Result<Uuid, FailureKind> {
    let bytes = axum::body::to_bytes(body, MAX_EDIT_BYTES)
        .await
        .map_err(|_| FailureKind::InvalidRequest)?;
    let Checked { request, base_hash } = check(&bytes)?;
    let scoped = scope_of(state, principal, correlation, site, &request.source_ref).await?;
    let operation = WriteOperation::Update(UpdateFile {
        path: VaultWritePath::parse(&scoped.path)
            .map_err(|_| FailureKind::UpstreamInvalidResponse)?,
        base_hash,
        blob: request.content_blob.clone(),
    });
    let mut command = notes_writes::command(
        NotesConnectionId(scoped.connection_id),
        operation,
        &request.idempotency_key,
        &format!("site:{site}"),
    )?;
    let mut fingerprint = format!("site-edits/{site}\n").into_bytes();
    fingerprint.extend_from_slice(&bytes);
    let intake = Intake {
        route: EDIT_LEDGER_ROUTE,
        kind: notes_writes::FILE_WRITE_KIND,
        audit_action: notes_writes::FILE_WRITE_KIND,
        key: &request.idempotency_key,
        fingerprint: &fingerprint,
        correlation,
        reporter: Some(notes_writes::NOTES_PRODUCER),
    };
    let row = SiteEditRow {
        site_id: site,
        source_ref: &request.source_ref,
        revision: &request.content_hash,
    };
    let minted = admit(state, principal, &intake, Some(&row), |operation, now| {
        command.operation_id = ratatoskr_identifiers::OperationId(operation.operation_id);
        notes_writes::envelope(&command, principal.user_id, now).map(Some)
    })
    .await?;
    let (Minted::Fresh(operation_id) | Minted::Replayed(operation_id)) = minted;
    Ok(operation_id)
}

/// `GET /v1/sites/{site}/conflicts/{conflict_id}`.
pub(crate) async fn conflict(
    state: &Arc<ApiState>,
    context: &RequestContext,
    principal: Principal,
    site: &str,
    conflict_id: &str,
) -> Response {
    let correlation = context.correlation_id.to_string();
    match read_conflict(state, principal, &correlation, site, conflict_id).await {
        Ok(conflict) => (StatusCode::OK, axum::Json(conflict)).into_response(),
        Err(refusal) => platform_http::reject(refusal),
    }
}

/// The conflict view of `ratatoskr-notes` that Edge reads.
#[derive(Debug, serde::Deserialize)]
struct NotesConflict {
    conflict_id: Uuid,
    operation_id: Uuid,
    connection_id: Uuid,
    path: String,
    state: String,
    #[serde(default)]
    texts: SiteConflictTexts,
    #[serde(default)]
    texts_over_limit: Vec<String>,
    base_hash: Option<String>,
    current_hash: Option<String>,
    writer_hash: Option<String>,
    merge_proposal: Option<SiteMergeProposal>,
}

async fn read_conflict(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
    conflict_id: &str,
) -> Result<SiteConflict, FailureKind> {
    let conflict_id = Uuid::parse_str(conflict_id).map_err(|_| FailureKind::NotFound)?;
    let claims = site_claims(principal, correlation, site);
    let notes: NotesConflict = notes_json(
        state,
        http::Method::GET,
        &format!("/v1/notes/conflicts/{conflict_id}"),
        &claims,
        Body::empty(),
    )
    .await?;
    let query = serde_urlencoded_pair(&notes.connection_id.to_string(), &notes.path);
    let request = loopback(
        state,
        crate::sites::SERVICE,
        http::Method::GET,
        &format!("/internal/sites/{site}/scope?{query}"),
        claims,
        Body::empty(),
    )?;
    let scoped: InScope = state
        .gateway
        .fetch_json(crate::sites::SERVICE, request, true, MAX_ANSWER_BYTES)
        .await?;
    Ok(SiteConflict {
        conflict_id: notes.conflict_id,
        operation_id: notes.operation_id,
        source_ref: scoped.source_ref,
        state: notes.state,
        texts: notes.texts,
        texts_over_limit: notes.texts_over_limit,
        base_hash: notes.base_hash,
        current_hash: notes.current_hash,
        writer_hash: notes.writer_hash,
        merge_proposal: notes.merge_proposal,
    })
}

/// `connection_id=<id>&path=<path>`, with the path percent-encoded.
fn serde_urlencoded_pair(connection_id: &str, path: &str) -> String {
    let encoded: String = path
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect();
    format!("connection_id={connection_id}&path={encoded}")
}
