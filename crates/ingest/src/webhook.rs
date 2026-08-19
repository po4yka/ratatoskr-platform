//! The generic webhook adapter.
//!
//! One route, and the whole of `ARCHITECTURE.md` S9 behind it. A third party pushes a signal, and
//! six steps later it is a durable operation and a command on the bus — the same operation and the
//! same command a client would have produced through `POST /v2/captures`, because a webhook is a
//! second door into one room rather than a second room.
//!
//! # What "generic" means here
//!
//! It means Platform defines the shape and a source sends it. It does NOT mean Platform parses
//! whatever a provider happens to emit: per-provider parsing is the provider-specific work S9
//! excludes from this process by definition, and a normalizer that accepts arbitrary JSON is an
//! unbounded value from an untrusted caller landing in a command that a consumer trusts
//! (`THREAT_MODEL.md`, ingress abuse). [`WebhookSignal`] is the shape, and it is closed.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use http::HeaderMap;
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_eventing::{MessageClass, Outbox, Subject};
use platform_idempotency::{Digest, Outcome};
use uuid::Uuid;

use crate::IngestState;
use crate::source::{SourceError, WebhookSource};

/// The route, as stored in the idempotency ledger.
///
/// Without the source identifier, because the ledger's `route` column is a bounded path and a UUID
/// does not fit its grammar. The source is folded into the key instead (see [`dedup_key`]), which
/// scopes the reservation more tightly than the column could.
const ROUTE: &str = "/v2/ingest/webhooks";

/// The header a source's own event identifier arrives in.
///
/// `Idempotency-Key` rather than a bespoke header: `INTERFACES.md` requires it on a replayable
/// mutation, every other mutating route in this repository already uses it, and a provider that
/// retries a delivery is doing exactly what the header exists for. One mechanism, not two.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// The longest identifier a source may choose for one delivery.
///
/// Shorter than the 255 the client-facing route allows, because the operation records this key
/// QUALIFIED by its source — a 36-character UUID and a separator — and
/// `operations.operations.idempotency_key` is bounded at 255. A delivery identifier is a UUID or a
/// counter in practice, so the difference is not one a real sender meets.
const MAX_EXTERNAL_ID: usize = 200;

/// What a source sends.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WebhookSignal {
    /// The address to capture. `http` or `https`, with a host, at most 2048 characters.
    pub url: String,
}

/// What a source gets back.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SignalAccepted {
    /// The operation created for this signal. The owner of the source can read it at
    /// `GET /v2/operations/{operation_id}`; the source itself cannot, because it has no session.
    pub operation_id: Uuid,
    /// Always `accepted` here. Present so a caller never has to infer it from the status code.
    pub status: &'static str,
}

/// `POST /v2/ingest/webhooks/{source_id}`.
///
/// Authenticates the source, bounds the signal, then hands the transaction to [`accept`].
pub async fn receive(
    State(state): State<Arc<IngestState>>,
    Path(source_id): Path<Uuid>,
    headers: HeaderMap,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let Some(presented) = platform_identity::bearer(&headers) else {
        return platform_http::reject(FailureKind::Unauthenticated);
    };

    let source = match crate::authenticate(state.database.pool(), presented).await {
        Ok(Some(source)) => source,
        Ok(None) => return platform_http::reject(FailureKind::Unauthenticated),
        Err(SourceError::UnknownTarget { source_id, target }) => {
            tracing::error!(%source_id, %target, "a registered source routes nowhere");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
        Err(error) => {
            // A database failure is not an authentication failure and must not be reported as one.
            tracing::error!(%error, "the source could not be resolved");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    // The credential decides who this is; the path only says who the caller MEANT. Requiring the
    // two to agree stops one source's credential being replayed at another source's URL, which
    // would otherwise file its signals under the wrong owner.
    if source.source_id != source_id {
        return platform_http::reject(FailureKind::Unauthenticated);
    }

    let (key, signal) = match parse(&headers, &body) {
        Ok(parsed) => parsed,
        Err(kind) => return platform_http::reject(kind),
    };

    // The correlation the middleware already minted for this request (ADR-0007). `Option` because
    // a unit test may call the handler without the middleware; in production it is always present.
    let correlation = context.map_or_else(
        || platform_telemetry::correlation::mint_correlation().to_string(),
        |axum::Extension(context)| context.correlation_id.to_string(),
    );

    accept(&state, &source, &key, &signal, &body, &correlation).await
}

/// Reserve, create, enqueue, complete — in one transaction, so a crash at any point leaves all four
/// or none.
///
/// Split from [`receive`] along the boundary that means something: everything before it decides
/// whether this signal is one we accept, and this does the work.
async fn accept(
    state: &IngestState,
    source: &WebhookSource,
    key: &str,
    signal: &WebhookSignal,
    body: &[u8],
    correlation: &str,
) -> Response {
    let qualified = qualify(source.source_id, key);
    let now = jiff::Timestamp::now();
    let target = source.target;
    let Ok(subject) = Subject::new(MessageClass::Command, target.command_type()) else {
        tracing::error!(
            command = target.command_type(),
            "the command subject is not constructible"
        );
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let Ok(mut transaction) = state.database.pool().begin().await else {
        tracing::error!("a transaction could not be started");
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let reservation = platform_idempotency::reserve(
        &mut transaction,
        source.owner_user_id,
        ROUTE,
        target.operation_kind(),
        Digest::of_key(&qualified),
        Digest::of_body(body),
        now,
        state.idempotency_ttl,
    )
    .await;

    let record_id = match reservation {
        Ok(reservation) => match reservation.outcome() {
            Outcome::Proceed(record_id) => record_id,
            // The provider redelivered. It gets the operation the first delivery created, which is
            // what stops an at-least-once webhook becoming a duplicate capture.
            Outcome::Replay(operation_id) => return accepted(operation_id),
            Outcome::Refuse => return platform_http::reject(FailureKind::IdempotencyConflict),
        },
        Err(error) => {
            tracing::error!(%error, "the signal could not be reserved");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let operation = match platform_operations::accept(
        &mut *transaction,
        source.owner_user_id,
        target.operation_kind(),
        correlation,
        Some(&qualified),
        now,
    )
    .await
    {
        Ok(operation) => operation,
        Err(error) => {
            tracing::error!(%error, "the operation could not be accepted");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let payload = platform_eventing::Command {
        command_type: target.command_type(),
        operation_id: operation.operation_id,
        principal: source.owner_user_id,
        correlation_id: correlation,
        idempotency_key: key,
        requested_at: now,
    }
    .envelope(serde_json::json!({ "url": signal.url }));

    if let Err(error) = Outbox::enqueue(
        &mut *transaction,
        Uuid::now_v7(),
        &subject,
        &payload,
        Some(operation.operation_id),
        now,
    )
    .await
    {
        tracing::error!(%error, "the command could not be enqueued");
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
        tracing::error!(%error, "the ledger record could not be completed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    if let Err(error) = transaction.commit().await {
        // Nothing happened. The provider's next delivery of the same signal gets a clean first
        // attempt, which is exactly what an at-least-once webhook will produce.
        tracing::error!(%error, "the signal transaction could not be committed");
        return platform_http::reject(FailureKind::RequestTimeout);
    }

    tracing::info!(
        source = %source.label,
        operation_id = %operation.operation_id,
        target = %target,
        "a signal was accepted",
    );
    accepted(operation.operation_id)
}

/// The identifier of one delivery: the source, then the identifier the source chose.
///
/// Two sources owned by one user share an actor, a route and an operation kind, so every scope
/// this repository enforces — the ledger's and `operations.operations`' own unique index — would
/// treat their deliveries as the same one. `1`, `42` and a Unix second are all identifiers a sender
/// might choose, and two senders choose them without consulting each other.
///
/// Qualifying the key is what separates them, and it is used for BOTH the reservation and the
/// operation record, so the key an operation says it was created for is the key it was actually
/// reserved under.
fn qualify(source_id: Uuid, external_id: &str) -> String {
    format!("{source_id}:{external_id}")
}

/// Read the source's event identifier and the signal, or say which client error this is.
///
/// The body is bounded before it is parsed only by the listener's own limit; everything this
/// function accepts is then bounded by shape.
fn parse(headers: &HeaderMap, body: &[u8]) -> Result<(String, WebhookSignal), FailureKind> {
    let key = headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= MAX_EXTERNAL_ID)
        .ok_or(FailureKind::MissingIdempotencyKey)?
        .to_owned();

    let signal: WebhookSignal =
        serde_json::from_slice(body).map_err(|_| FailureKind::InvalidRequest)?;
    if !platform_core::address::is_capturable(&signal.url) {
        return Err(FailureKind::InvalidRequest);
    }
    Ok((key, signal))
}

/// The 202 body.
fn accepted(operation_id: Uuid) -> Response {
    (
        http::StatusCode::ACCEPTED,
        Json(SignalAccepted {
            operation_id,
            status: "accepted",
        }),
    )
        .into_response()
}

/// How this route is described in the generated `OpenAPI` document.
pub const DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v2/ingest/webhooks/{source_id}",
    operation_id: "receiveWebhookSignal",
    summary: "Push a signal from a registered source",
    description: "\
Accepts one signal from a registered webhook source and turns it into a durable operation for the \
user who owns that source. It is not a client route: the credential authenticates a machine, and \
the operation it creates is readable by the source's owner rather than by the source.\n\n\
`Idempotency-Key` carries the source's OWN identifier for the event — a delivery id, a message id, \
whatever the sender already uses to recognise a redelivery. It is required. Delivering the same \
identifier with the same body again returns the operation the first delivery created, which is \
what stops an at-least-once webhook becoming duplicate work; the same identifier with a different \
body is refused. Identifiers are scoped per source, so two sources need not coordinate.\n\n\
The body shape is fixed by this API, not by the sender: a source that cannot emit it needs a shim \
in front, because parsing a provider's own format is provider-specific work that belongs in a \
provider's own service.",
    tag: "ingest",
    security: Security::SourceToken,
    parameters: &[
        Parameter {
            name: "source_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The source this signal is for. It must be the source the credential \
                          belongs to; a mismatch is refused as if the credential were unknown.",
        },
        Parameter {
            name: "Idempotency-Key",
            location: In::Header,
            required: true,
            format: None,
            description: "The sender's own identifier for this event, 1 to 200 characters. It is \
                          scoped to this source, so two sources need not agree on a format.",
        },
    ],
    request: Some(Payload::Json("WebhookSignal")),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "Accepted durably, or already accepted under this identifier. The body \
                          carries the operation either way.",
            payload: Some(Payload::Json("SignalAccepted")),
        },
        ResponseDoc {
            status: 400,
            description: "No `Idempotency-Key`, a body that is not readable, or an address this \
                          API will not accept.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, an unknown or disabled one, or one that belongs to a \
                          different source than the path names.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The identifier is in use for a different body, or an earlier delivery \
                          of it has not finished.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written; redelivering \
                          the same signal is safe and is the intended response.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
