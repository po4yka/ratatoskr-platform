//! Submitting a capture.
//!
//! The one route that exercises the whole stack, and the reason milestones 2 to 4 were built the way
//! they were: the idempotency reservation, the operation record and the outbox row are written in
//! ONE transaction, so a crash at any point leaves all three or none.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse as _, Response};
use http::HeaderMap;
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_eventing::{CaptureSource, ContentCaptureCommand, MessageClass, Subject};
use ratatoskr_event_envelope::{
    CommandEnvelope, CommandPayload, EnvelopeSchemaVersion, ProducerName,
};
use ratatoskr_identifiers::{
    BlobRef, CommandId, ContentDigest, DigestAlgorithm, DigestHex, Extensions, OperationId,
    TenantRef, UserId,
};
use ratatoskr_social_contracts::{
    AcquisitionMethod, PostPermalink, SavedAuthority, SocialCaptureProvider, SocialCaptureRequested,
};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use platform_identity::SessionKind;

use crate::intake::{Intake, Prepared, accept};
use crate::{ApiState, Principal};

/// The route, as stored in the idempotency ledger.
const ROUTE: &str = "/v1/captures";

/// The blob route, as stored in the idempotency ledger. A different route string from [`ROUTE`], so
/// the same key on the two routes can never be taken for one request.
const BLOB_ROUTE: &str = "/v1/captures/blobs";

/// The only service whose stored bytes a client may name (CONTRACTS.md S11).
const TELEGRAM_BLOB_OWNER: &str = "ratatoskr-telegram";

/// The largest blob a capture may name: the extractor's own ceiling for a PDF.
const MAX_BLOB_CAPTURE_BYTES: u64 = 52_428_800;

/// What the submitted work is. Present tense: a kind names an activity, not a completed fact.
const OPERATION_KIND: &str = "content.capture.submit";

/// The command this route emits. The extractor is its consumer.
const COMMAND_TYPE: &str = "content.capture.requested.v1";

/// The header `INTERFACES.md` requires on a replayable mutation.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// What a client submits.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SubmitCapture {
    /// The address to capture. `http` or `https`, with a host, at most 2048 characters.
    pub url: String,
    /// Provenance supplied by an explicit browser social capture, when this is a social permalink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub social: Option<SocialCaptureProvenance>,
}

/// The social provenance a browser extension must assert explicitly.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SocialCaptureProvenance {
    /// The provider whose public permalink is captured.
    pub provider: SocialCaptureProvider,
    /// The browser's capture instant.
    pub captured_at: ratatoskr_identifiers::WireTimestamp,
    /// Must be `browser_extension`; a caller cannot claim another acquisition lane here.
    pub acquisition: AcquisitionMethod,
    /// Must be `explicit_user_capture`; this path never claims provider Saved state.
    pub saved_authority: SavedAuthority,
}

/// What a client gets back.
///
/// `ARCHITECTURE.md` S5.1: the API acknowledges durable ACCEPTANCE, not completion. The body
/// therefore carries an operation to poll and nothing that could be mistaken for a result.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct CaptureAccepted {
    /// The operation to poll at `/v1/operations/{id}`.
    pub operation_id: Uuid,
    /// Always `accepted` here. Present so a client never has to infer it from the status code.
    pub status: &'static str,
}

/// The members of `SubmitCapture`'s sibling for stored bytes: the blob a client names by reference.
///
/// Top-level members this struct does not name (the Telegram bot sends an `origin` that has no
/// meaning here) are tolerated and ignored, so the bot can add routing hints without a Platform
/// release. The blob itself is strict: a member the contract does not define is refused.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SubmitBlobCapture {
    /// The stored bytes to extract. Owned by the Telegram service, between 1 byte and 50 MiB.
    pub blob: BlobRef,
}

/// `POST /v1/captures`.
///
/// Refuses rather than guesses: no `Idempotency-Key` is a client error, because a replayable
/// mutation without one is an unprotected write that only looks safe.
pub async fn submit(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    headers: HeaderMap,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let (key, submit) = match parse(&headers, &body) {
        Ok(parsed) => parsed,
        Err(kind) => return platform_http::reject(kind),
    };

    let correlation = crate::correlation_of(context);
    let intake = Intake {
        route: ROUTE,
        kind: OPERATION_KIND,
        audit_action: OPERATION_KIND,
        key: &key,
        fingerprint: &body,
        correlation: &correlation,
    };
    accept(&state, principal, &intake, accepted, |operation, now| {
        prepare_url(operation, &principal, &intake, now, &submit)
    })
    .await
}

/// `POST /v1/captures/blobs`.
///
/// The Telegram bot stores a PDF in its own blob root and asks for it to be captured by reference.
/// The store is content-addressed and deduplicated across users, so two things stand between a
/// caller and someone else's bytes: only a Telegram Mini App session may call this at all, and the
/// blob must be one the Telegram service owns. The digest is a 256-bit hash nobody can guess, which
/// is the third.
pub async fn submit_blob(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    headers: HeaderMap,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    // Before anything about the request is read: a session of another kind learns nothing from the
    // body it sent.
    if principal.kind != SessionKind::TelegramMiniApp {
        return platform_http::reject(FailureKind::Forbidden);
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(kind) => return platform_http::reject(kind),
    };
    let Ok(submit) = serde_json::from_slice::<SubmitBlobCapture>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    if !(1..=MAX_BLOB_CAPTURE_BYTES).contains(&submit.blob.length_bytes) {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    if submit.blob.owner_service.as_str() != TELEGRAM_BLOB_OWNER {
        return platform_http::reject(FailureKind::Forbidden);
    }

    let correlation = crate::correlation_of(context);
    let intake = Intake {
        route: BLOB_ROUTE,
        kind: OPERATION_KIND,
        audit_action: OPERATION_KIND,
        key: &key,
        fingerprint: &body,
        correlation: &correlation,
    };
    accept(&state, principal, &intake, accepted, |operation, now| {
        let command_id = Uuid::now_v7();
        Ok(Prepared {
            subject: command_subject(COMMAND_TYPE)?,
            command_id,
            payload: content_command(
                command_id,
                operation,
                &principal,
                &intake,
                now,
                CaptureSource::Blob(&submit.blob),
            )?,
        })
    })
    .await
}

/// The caller's idempotency key, or the client error for its absence.
fn idempotency_key(headers: &HeaderMap) -> Result<String, FailureKind> {
    headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 255)
        .map(str::to_owned)
        .ok_or(FailureKind::MissingIdempotencyKey)
}

/// Read the idempotency key and the body, or say which client error this is.
///
/// The body is parsed from raw bytes rather than through `Json<T>`, because the fingerprint must be
/// taken over exactly what the client sent: re-serializing a parsed value would make two
/// byte-different requests look identical to the ledger.
fn parse(
    headers: &HeaderMap,
    body: &axum::body::Bytes,
) -> Result<(String, SubmitCapture), FailureKind> {
    let key = idempotency_key(headers)?;

    let submit: SubmitCapture =
        serde_json::from_slice(body).map_err(|_| FailureKind::InvalidRequest)?;
    // Both grammars: the route's own address check, and the contract's, which the command carries.
    if !platform_core::address::is_capturable(&submit.url)
        || !platform_eventing::is_capture_url(&submit.url)
    {
        return Err(FailureKind::InvalidRequest);
    }
    if let Some(social) = &submit.social
        && (social.acquisition != AcquisitionMethod::BrowserExtension
            || social.saved_authority != SavedAuthority::ExplicitUserCapture
            || !is_provider_permalink(&submit.url, social.provider))
    {
        return Err(FailureKind::InvalidRequest);
    }
    Ok((key, submit))
}

/// Checks that a typed provenance owner agrees with the public URL, without fetching it.
fn is_provider_permalink(url: &str, provider: SocialCaptureProvider) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    match provider {
        SocialCaptureProvider::X => matches!(
            host.as_str(),
            "x.com"
                | "www.x.com"
                | "mobile.x.com"
                | "twitter.com"
                | "www.twitter.com"
                | "mobile.twitter.com"
        ),
        SocialCaptureProvider::Instagram => {
            matches!(host.as_str(), "instagram.com" | "www.instagram.com")
        }
        SocialCaptureProvider::Threads => {
            matches!(host.as_str(), "threads.net" | "www.threads.net")
        }
        _ => false,
    }
}

/// The subject of a command type, or the internal failure of a type that is not one.
fn command_subject(command_type: &str) -> Result<Subject, FailureKind> {
    Subject::new(MessageClass::Command, command_type).map_err(|_| {
        tracing::error!(
            command = command_type,
            "the command subject is not constructible"
        );
        FailureKind::RequestTimeout
    })
}

/// Selects the only command representation that belongs to the accepted capture.
fn prepare_url(
    operation: &platform_operations::Operation,
    principal: &Principal,
    intake: &Intake<'_>,
    now: jiff::Timestamp,
    submit: &SubmitCapture,
) -> Result<Prepared, FailureKind> {
    let command_id = Uuid::now_v7();
    match &submit.social {
        Some(social) => Ok(Prepared {
            subject: command_subject(social.provider.command_subject())?,
            command_id,
            payload: social_command(
                command_id,
                operation,
                principal,
                intake.key,
                now,
                &submit.url,
                social,
            )?,
        }),
        None => Ok(Prepared {
            subject: command_subject(COMMAND_TYPE)?,
            command_id,
            payload: content_command(
                command_id,
                operation,
                principal,
                intake,
                now,
                CaptureSource::Url(&submit.url),
            )?,
        }),
    }
}

/// The typed `content.capture.requested.v1` envelope, shared with the webhook adapter.
fn content_command(
    command_id: Uuid,
    operation: &platform_operations::Operation,
    principal: &Principal,
    intake: &Intake<'_>,
    now: jiff::Timestamp,
    source: CaptureSource<'_>,
) -> Result<serde_json::Value, FailureKind> {
    ContentCaptureCommand {
        command_id,
        operation_id: operation.operation_id,
        principal: principal.user_id,
        correlation_id: intake.correlation,
        idempotency_key: intake.key,
        issued_at: now,
        source,
    }
    .envelope()
    .map_err(|error| {
        tracing::error!(%error, "the capture command could not be built");
        FailureKind::RequestTimeout
    })
}

/// Builds the canonical contract envelope for the social service that owns an explicit capture.
fn social_command(
    command_id: Uuid,
    operation: &platform_operations::Operation,
    principal: &Principal,
    idempotency_key: &str,
    now: jiff::Timestamp,
    url: &str,
    social: &SocialCaptureProvenance,
) -> Result<serde_json::Value, FailureKind> {
    let original_permalink = PostPermalink::parse(url).map_err(|_| FailureKind::InvalidRequest)?;
    let hex = format!("{:x}", Sha256::digest(idempotency_key.as_bytes()));
    let hex = DigestHex::parse(&hex).map_err(|_| FailureKind::RequestTimeout)?;
    let social_payload = SocialCaptureRequested {
        operation_id: OperationId(operation.operation_id),
        idempotency_key: ContentDigest {
            algorithm: DigestAlgorithm::Sha256,
            hex,
        },
        original_permalink,
        captured_at: social.captured_at,
        provider: social.provider,
        acquisition: social.acquisition,
        saved_authority: social.saved_authority,
        extensions: Extensions::new(),
    };
    let serde_json::Value::Object(payload) =
        serde_json::to_value(social_payload).map_err(|_| FailureKind::RequestTimeout)?
    else {
        return Err(FailureKind::RequestTimeout);
    };
    let operation_ref = OperationId(operation.operation_id).as_entity_ref();
    let envelope = CommandEnvelope {
        command_id: CommandId(command_id),
        command_type: SocialCaptureRequested::command_type(),
        issued_at: ratatoskr_identifiers::WireTimestamp::from_jiff(now),
        producer: ProducerName::parse("ratatoskr-platform")
            .map_err(|_| FailureKind::RequestTimeout)?,
        aggregate_id: operation_ref.clone(),
        correlation_id: operation_ref,
        causation_id: None,
        tenant_id: Some(TenantRef::of_user(UserId(principal.user_id))),
        schema_version: EnvelopeSchemaVersion::CURRENT,
        payload,
        extensions: Extensions::new(),
    };
    serde_json::to_value(envelope).map_err(|_| FailureKind::RequestTimeout)
}

/// The 202 body. `ARCHITECTURE.md` S5.3: `202 Accepted` for asynchronous work.
fn accepted(operation_id: Uuid) -> Response {
    (
        http::StatusCode::ACCEPTED,
        Json(CaptureAccepted {
            operation_id,
            status: "accepted",
        }),
    )
        .into_response()
}

/// How this route is described in the generated `OpenAPI` document.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: ROUTE,
    operation_id: "submitCapture",
    summary: "Submit an address for capture",
    description: "\
Accepts the address durably and returns the operation that tracks it. It does NOT return a result: \
the work happens elsewhere, and `GET /v1/operations/{operation_id}` is where its outcome appears.\n\n\
`Idempotency-Key` is required, not optional. A capture is a replayable mutation, and a retry \
without a key is a second operation that looks like the first. Retrying with the same key and the \
same body returns the ORIGINAL operation; the same key with a different body is refused, because \
honouring it would silently replace the meaning of a request already sent.\n\n\
The address is checked only for a usable scheme and host. Fetching it, following its redirects and \
bounding what it returns belong to the service that opens the connection, not to this one.",
    tag: "captures",
    security: Security::Session,
    parameters: &[Parameter {
        name: "Idempotency-Key",
        location: In::Header,
        required: true,
        format: None,
        description: "A client-chosen key, 1 to 255 characters, unique per distinct request. It is \
                      hashed before it is stored, so it may carry meaning the client considers \
                      private.",
    }],
    request: Some(Payload::Json("SubmitCapture")),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "Accepted durably. The body carries the operation to poll.",
            payload: Some(Payload::Json("CaptureAccepted")),
        },
        ResponseDoc {
            status: 400,
            description: "No `Idempotency-Key`, a body that is not readable, or an address this \
                          API will not accept. The `code` in the envelope distinguishes them.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "This caller has spent its request allowance. Retryable: the allowance \
                          refills continuously, so waiting is the fix.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The key is in use for a different body, or an earlier attempt with it \
                          has not finished.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written; retrying with \
                          the same key is safe and is the intended response.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How the blob route is described in the generated `OpenAPI` document.
pub const BLOB_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: BLOB_ROUTE,
    operation_id: "submitBlobCapture",
    summary: "Submit stored bytes for capture",
    description: "\
Accepts, durably, a request to extract a document from bytes another service already stored, named \
by reference rather than carried. It returns the operation that tracks the work and no result: \
`GET /v1/operations/{operation_id}` is where the outcome appears.\n\n\
Only the Telegram bot's session may call this route, and only for bytes the Telegram service owns \
(`owner_service` must be `ratatoskr-telegram`). The blob must declare between 1 byte and 50 MiB. \
Platform does not judge whether the media type can be extracted; the extractor reports that on the \
operation.\n\n\
`Idempotency-Key` is required, exactly as on `POST /v1/captures`: the same key and body return the \
ORIGINAL operation, and the same key with a different body is refused. Members of the body other \
than `blob` are ignored.",
    tag: "captures",
    security: Security::Session,
    parameters: &[Parameter {
        name: "Idempotency-Key",
        location: In::Header,
        required: true,
        format: None,
        description: "A client-chosen key, 1 to 255 characters, unique per distinct request. It is \
                      hashed before it is stored, so it may carry meaning the client considers \
                      private.",
    }],
    request: Some(Payload::Json("SubmitBlobCapture")),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "Accepted durably. The body carries the operation to poll.",
            payload: Some(Payload::Json("CaptureAccepted")),
        },
        ResponseDoc {
            status: 400,
            description: "No `Idempotency-Key`, a body that is not readable, or a blob outside \
                          the accepted size. The `code` in the envelope distinguishes them.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 403,
            description: "The session is not a Telegram Mini App session, or the blob is owned by \
                          another service.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The key is in use for a different body, or an earlier attempt with it \
                          has not finished.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 429,
            description: "This caller has spent its request allowance. Retryable: the allowance \
                          refills continuously, so waiting is the fix.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written; retrying with \
                          the same key is safe and is the intended response.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
