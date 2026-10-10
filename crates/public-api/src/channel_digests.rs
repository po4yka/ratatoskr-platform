//! The channel-digest routes (XR-021 CONTRACTS.md S08).
//!
//! Two command routes hand work to `ratatoskr-channel-digests` through the outbox: a subscription
//! change and an on-demand digest run. Each is a Platform operation, accepted in the same
//! transaction that writes its idempotency record, its audit record and the typed
//! `CommandEnvelope` the digest service consumes.
//!
//! Three read routes go the other way, through a typed client with a fixed path per route. The
//! generic gateway proxy is deliberately NOT used: it strips `Authorization` (correctly, for the
//! domain APIs it fronts) and the digest API is bearer-protected, so the proxy would either fail
//! every request or, if it were taught to forward a credential, expose that API to any session.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse as _, Response};
use http::header::{AUTHORIZATION, CACHE_CONTROL};
use http::{HeaderMap, HeaderValue, Method};
use platform_api_doc::{
    In, Method as DocMethod, Parameter, Payload, ResponseDoc, RouteDoc, Security,
};
use platform_core::FailureKind;
use platform_eventing::{MessageClass, PLATFORM_PRODUCER, Subject};
use ratatoskr_channel_digest_contracts::{
    ChannelDigestIdempotencyKey, ChannelDigestResultId, ChannelDigestResultPage,
    ChannelDigestResultView, ChannelDigestRunId, ChannelDigestRunRequested,
    ChannelDigestRunTrigger, ChannelDigestSubscriptionPage, ChannelDigestSubscriptionSetRequested,
    ChannelUsername, DigestWindow, HEADER_OWNER_ID, OutputLanguage, SubscriptionDesiredState,
};
use ratatoskr_event_envelope::{
    CommandEnvelope, CommandPayload, EnvelopeSchemaVersion, ProducerName,
};
use ratatoskr_identifiers::{
    CommandId, EntityRef, Extensions, OperationId, TenantRef, UserId, WireTimestamp,
};
use secrecy::{ExposeSecret as _, SecretString};
use uuid::Uuid;

use crate::intake::{Intake, Prepared, accept};
use crate::{ApiState, Principal};

const SUBSCRIPTION_ROUTE: &str = "/v1/channel-digests/subscriptions/{channel_username}";
const SUBSCRIPTIONS_ROUTE: &str = "/v1/channel-digests/subscriptions";
const RUNS_ROUTE: &str = "/v1/channel-digests/runs";
const RESULTS_ROUTE: &str = "/v1/channel-digests/results";
const RESULT_ROUTE: &str = "/v1/channel-digests/results/{result_id}";

const SUBSCRIPTION_KIND: &str = "channel_digest.subscription.set";
const RUN_KIND: &str = "channel_digest.run";
const SUBSCRIPTION_AUDIT: &str = "channel_digest.subscription.set";
const RUN_AUDIT: &str = "channel_digest.run.request";

const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// How far back an on-demand digest looks: the 24 hours before it was accepted.
const RUN_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// The largest digest-service answer Edge reads.
const DIGESTS_RESPONSE_BYTES: usize = 256 * 1024;

const MAX_PAGE_SIZE: u32 = 100;

/// What a client submits to set a subscription's state.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetSubscription {
    /// The state the subscription should end up in.
    pub desired_state: SubscriptionDesiredState,
}

/// What a client submits to ask for a digest now.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestRun {
    /// The language of the recap. Required: there is no default language.
    pub output_language: OutputLanguage,
}

/// What a client gets back from either command route: an operation to poll, never a result.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct ChannelDigestAccepted {
    /// The operation to poll at `/v1/operations/{id}`.
    pub operation_id: Uuid,
    /// Always `accepted` here.
    pub status: &'static str,
}

fn accepted(operation_id: Uuid) -> Response {
    (
        http::StatusCode::ACCEPTED,
        Json(ChannelDigestAccepted {
            operation_id,
            status: "accepted",
        }),
    )
        .into_response()
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, FailureKind> {
    headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 255)
        .map(str::to_owned)
        .ok_or(FailureKind::MissingIdempotencyKey)
}

/// `PUT /v1/channel-digests/subscriptions/{channel_username}`.
///
/// The username in the path must already be the canonical spelling: it is not lower-cased on the
/// way in, because two spellings of one channel would be two requests with two operations.
pub async fn set_subscription(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(username): Path<String>,
    headers: HeaderMap,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let Ok(channel_username) = ChannelUsername::parse(&username) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(kind) => return platform_http::reject(kind),
    };
    let Ok(submitted) = serde_json::from_slice::<SetSubscription>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };

    // The path names which channel the body acts on, so it is part of what makes two requests the
    // same: the same key and body for another channel must be refused, not replayed.
    let mut fingerprint = username.into_bytes();
    fingerprint.push(b'\n');
    fingerprint.extend_from_slice(&body);
    let correlation = crate::correlation_of(context);
    let intake = Intake {
        route: SUBSCRIPTION_ROUTE,
        kind: SUBSCRIPTION_KIND,
        audit_action: SUBSCRIPTION_AUDIT,
        key: &key,
        fingerprint: &fingerprint,
        correlation: &correlation,
    };
    accept(&state, principal, &intake, accepted, |operation, now| {
        let operation_id = OperationId(operation.operation_id);
        let command = ChannelDigestSubscriptionSetRequested {
            operation_id,
            owner: TenantRef::of_user(UserId(principal.user_id)),
            idempotency_key: operation_key(operation.operation_id)?,
            channel_username,
            desired_state: submitted.desired_state,
            extensions: Extensions::new(),
        };
        command
            .validate_for_publish()
            .map_err(|_| FailureKind::RequestTimeout)?;
        envelope(
            &command,
            operation_id.as_entity_ref(),
            operation_id,
            principal.user_id,
            now,
        )
    })
    .await
}

/// `POST /v1/channel-digests/runs`.
pub async fn request_run(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    headers: HeaderMap,
    context: Option<axum::Extension<platform_http::RequestContext>>,
    body: axum::body::Bytes,
) -> Response {
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(kind) => return platform_http::reject(kind),
    };
    let Ok(submitted) = serde_json::from_slice::<RequestRun>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };

    let correlation = crate::correlation_of(context);
    let intake = Intake {
        route: RUNS_ROUTE,
        kind: RUN_KIND,
        audit_action: RUN_AUDIT,
        key: &key,
        fingerprint: &body,
        correlation: &correlation,
    };
    accept(&state, principal, &intake, accepted, |operation, now| {
        let operation_id = OperationId(operation.operation_id);
        let digest_run_id = ChannelDigestRunId::parse(&Uuid::now_v7().to_string())
            .map_err(|_| FailureKind::RequestTimeout)?;
        // Closed-open: the 24 hours before acceptance, ending exactly at the acceptance instant,
        // which is also the authority the trigger names.
        let end_at = WireTimestamp::from_jiff(now);
        let start_at = WireTimestamp::from_jiff(now - RUN_WINDOW);
        let command = ChannelDigestRunRequested {
            operation_id,
            owner: TenantRef::of_user(UserId(principal.user_id)),
            digest_run_id,
            idempotency_key: operation_key(operation.operation_id)?,
            window: DigestWindow::new(start_at, end_at).map_err(|_| FailureKind::RequestTimeout)?,
            output_language: submitted.output_language,
            trigger: ChannelDigestRunTrigger::OnDemand {
                accepted_at: end_at,
            },
            extensions: Extensions::new(),
        };
        command
            .validate_for_publish()
            .map_err(|_| FailureKind::RequestTimeout)?;
        let aggregate = EntityRef::parse(&format!("channel-digest-run:{digest_run_id}"))
            .map_err(|_| FailureKind::RequestTimeout)?;
        envelope(&command, aggregate, operation_id, principal.user_id, now)
    })
    .await
}

/// The logical request identity the digest service dedupes on: derived from the operation, so an
/// HTTP retry (replayed by the ledger) and a redelivery of the command collapse into one.
fn operation_key(operation_id: Uuid) -> Result<ChannelDigestIdempotencyKey, FailureKind> {
    ChannelDigestIdempotencyKey::parse(&format!("operation.{operation_id}")).map_err(|error| {
        tracing::error!(%error, "the digest idempotency key is not constructible");
        FailureKind::RequestTimeout
    })
}

/// The `CommandEnvelope` of one digest command, as the row the outbox stores.
fn envelope<P: CommandPayload>(
    payload: &P,
    aggregate: EntityRef,
    operation_id: OperationId,
    principal: Uuid,
    now: jiff::Timestamp,
) -> Result<Prepared, FailureKind> {
    fn internal<E>(class: &'static str) -> impl FnOnce(E) -> FailureKind {
        move |_| {
            tracing::error!(class, "a digest command could not be built");
            FailureKind::RequestTimeout
        }
    }
    let serde_json::Value::Object(body) =
        serde_json::to_value(payload).map_err(internal("payload"))?
    else {
        return Err(FailureKind::RequestTimeout);
    };
    let command_id = Uuid::now_v7();
    let envelope = CommandEnvelope {
        command_id: CommandId(command_id),
        command_type: P::command_type(),
        issued_at: WireTimestamp::from_jiff(now),
        producer: ProducerName::parse(PLATFORM_PRODUCER).map_err(internal("producer"))?,
        aggregate_id: aggregate,
        correlation_id: operation_id.as_entity_ref(),
        causation_id: None,
        tenant_id: Some(TenantRef::of_user(UserId(principal))),
        schema_version: EnvelopeSchemaVersion::CURRENT,
        payload: body,
        extensions: Extensions::new(),
    };
    Ok(Prepared {
        subject: Subject::new(MessageClass::Command, P::COMMAND_TYPE)
            .map_err(internal("subject"))?,
        command_id,
        payload: serde_json::to_value(envelope).map_err(internal("envelope"))?,
    })
}

/// The typed client for the `ratatoskr-channel-digests` read API: fixed paths, the service bearer
/// and the owner header, no redirects, and a bounded answer.
#[derive(Debug, Clone)]
pub struct ChannelDigestsClient {
    listener: SocketAddr,
    secret: SecretString,
}

impl ChannelDigestsClient {
    /// A client for the API listening on `listener`, authenticating with `secret`.
    #[must_use]
    pub const fn new(listener: SocketAddr, secret: SecretString) -> Self {
        Self { listener, secret }
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        state: &ApiState,
        owner: Uuid,
        path_and_query: &str,
    ) -> Result<T, FailureKind> {
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", self.secret.expose_secret()))
            .map_err(|_| FailureKind::UpstreamUnavailable)?;
        bearer.set_sensitive(true);
        let request = hyper::Request::builder()
            .method(Method::GET)
            .uri(format!("http://{}{path_and_query}", self.listener))
            .header(AUTHORIZATION, bearer)
            .header(HEADER_OWNER_ID, owner.to_string())
            .body(Body::empty())
            .map_err(|_| FailureKind::UpstreamUnavailable)?;
        state
            .gateway
            .fetch_json("channel_digests", request, true, DIGESTS_RESPONSE_BYTES)
            .await
    }
}

/// `page_size` of the two listings: 1 to 100, passed to the digest service unchanged.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageParams {
    page_size: Option<u32>,
}

impl PageParams {
    /// The query string the digest service receives, or the client error for an invalid size.
    fn query(&self) -> Result<String, FailureKind> {
        match self.page_size {
            None => Ok(String::new()),
            Some(size) if (1..=MAX_PAGE_SIZE).contains(&size) => Ok(format!("?page_size={size}")),
            Some(_) => Err(FailureKind::InvalidRequest),
        }
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Run one read: the configured client, or `UpstreamUnavailable` when the deployment has none.
async fn read<T: serde::Serialize + serde::de::DeserializeOwned>(
    state: &ApiState,
    principal: Principal,
    path_and_query: &str,
) -> Response {
    let Some(client) = &state.channel_digests else {
        return platform_http::reject(FailureKind::UpstreamUnavailable);
    };
    match client
        .get::<T>(state, principal.user_id, path_and_query)
        .await
    {
        Ok(view) => no_store(Json(view).into_response()),
        Err(kind) => platform_http::reject(kind),
    }
}

/// `GET /v1/channel-digests/subscriptions`.
pub async fn list_subscriptions(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    params: Result<Query<PageParams>, QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    match params.query() {
        Ok(query) => {
            read::<ChannelDigestSubscriptionPage>(
                &state,
                principal,
                &format!("/v1/subscriptions{query}"),
            )
            .await
        }
        Err(kind) => platform_http::reject(kind),
    }
}

/// `GET /v1/channel-digests/results`.
pub async fn list_results(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    params: Result<Query<PageParams>, QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    match params.query() {
        Ok(query) => {
            read::<ChannelDigestResultPage>(&state, principal, &format!("/v1/results{query}")).await
        }
        Err(kind) => platform_http::reject(kind),
    }
}

/// `GET /v1/channel-digests/results/{result_id}`.
pub async fn get_result(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(result_id): Path<String>,
) -> Response {
    let Ok(result_id) = ChannelDigestResultId::parse(&result_id) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    read::<ChannelDigestResultView>(&state, principal, &format!("/v1/results/{result_id}")).await
}

const IDEMPOTENCY_PARAMETER: Parameter = Parameter {
    name: "Idempotency-Key",
    location: In::Header,
    required: true,
    format: None,
    description: "A client-chosen key, 1 to 255 characters, unique per distinct request. It is \
                  hashed before it is stored.",
};

const PAGE_SIZE_PARAMETER: Parameter = Parameter {
    name: "page_size",
    location: In::Query,
    required: false,
    format: Some("uint32"),
    description: "Page size from 1 through 100; the digest service applies its own default when \
                  absent.",
};

const fn response(status: u16, description: &'static str, payload: &'static str) -> ResponseDoc {
    ResponseDoc {
        status,
        description,
        payload: Some(Payload::Json(payload)),
    }
}

/// `PUT /v1/channel-digests/subscriptions/{channel_username}`.
pub const SUBSCRIPTION_DOC: RouteDoc = RouteDoc {
    method: DocMethod::Put,
    path: SUBSCRIPTION_ROUTE,
    operation_id: "setChannelDigestSubscription",
    summary: "Subscribe to or unsubscribe from a public channel",
    description: "Accepts the change durably and returns the operation that tracks it; the digest \
                  service applies it and reports the outcome on the operation. The username must \
                  already be canonical: lowercase, 5 to 32 characters, starting with a letter. \
                  `Idempotency-Key` is required; retrying with the same key and body returns the \
                  original operation.",
    tag: "channel-digests",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "channel_username",
            location: In::Path,
            required: true,
            format: None,
            description: "The canonical public channel username.",
        },
        IDEMPOTENCY_PARAMETER,
    ],
    request: Some(Payload::Json("SetSubscription")),
    responses: &[
        response(
            202,
            "Accepted durably; the body carries the operation to poll.",
            "ChannelDigestAccepted",
        ),
        response(
            400,
            "A username, key or body that is not valid.",
            "ErrorEnvelope",
        ),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(
            409,
            "The key is in use for a different request.",
            "ErrorEnvelope",
        ),
        response(
            429,
            "This caller has spent its request allowance.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "A dependency did not answer in time; retrying with the same key is safe.",
            "ErrorEnvelope",
        ),
    ],
};

/// `POST /v1/channel-digests/runs`.
pub const RUN_DOC: RouteDoc = RouteDoc {
    method: DocMethod::Post,
    path: RUNS_ROUTE,
    operation_id: "requestChannelDigestRun",
    summary: "Request a digest of the last 24 hours",
    description: "Accepts the request durably and returns the operation that tracks it. The digest \
                  covers the 24 hours before acceptance. `Idempotency-Key` is required; retrying \
                  with the same key and body returns the original operation.",
    tag: "channel-digests",
    security: Security::Session,
    parameters: &[IDEMPOTENCY_PARAMETER],
    request: Some(Payload::Json("RequestRun")),
    responses: &[
        response(
            202,
            "Accepted durably; the body carries the operation to poll.",
            "ChannelDigestAccepted",
        ),
        response(
            400,
            "A key or body that is not valid; the language is required.",
            "ErrorEnvelope",
        ),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(
            409,
            "The key is in use for a different request.",
            "ErrorEnvelope",
        ),
        response(
            429,
            "This caller has spent its request allowance.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "A dependency did not answer in time; retrying with the same key is safe.",
            "ErrorEnvelope",
        ),
    ],
};

/// `GET /v1/channel-digests/subscriptions`.
pub const SUBSCRIPTIONS_DOC: RouteDoc = RouteDoc {
    method: DocMethod::Get,
    path: SUBSCRIPTIONS_ROUTE,
    operation_id: "listChannelDigestSubscriptions",
    summary: "List the caller's channel subscriptions",
    description: "The caller's subscriptions, newest first. Read from the digest service on the \
                  caller's behalf; the response is never cached.",
    tag: "channel-digests",
    security: Security::Session,
    parameters: &[PAGE_SIZE_PARAMETER],
    request: None,
    responses: &[
        response(
            200,
            "A page of subscriptions.",
            "ChannelDigestSubscriptionPage",
        ),
        response(400, "An invalid page size.", "ErrorEnvelope"),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(
            502,
            "The digest service returned an invalid response.",
            "ErrorEnvelope",
        ),
        response(
            503,
            "The digest service is unavailable or not configured.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "The digest service did not answer in time.",
            "ErrorEnvelope",
        ),
    ],
};

/// `GET /v1/channel-digests/results`.
pub const RESULTS_DOC: RouteDoc = RouteDoc {
    method: DocMethod::Get,
    path: RESULTS_ROUTE,
    operation_id: "listChannelDigestResults",
    summary: "List the caller's digest results",
    description: "Summaries of the caller's digest results, newest first, without recap content. \
                  Read from the digest service on the caller's behalf; the response is never \
                  cached.",
    tag: "channel-digests",
    security: Security::Session,
    parameters: &[PAGE_SIZE_PARAMETER],
    request: None,
    responses: &[
        response(
            200,
            "A page of result summaries.",
            "ChannelDigestResultPage",
        ),
        response(400, "An invalid page size.", "ErrorEnvelope"),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(
            502,
            "The digest service returned an invalid response.",
            "ErrorEnvelope",
        ),
        response(
            503,
            "The digest service is unavailable or not configured.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "The digest service did not answer in time.",
            "ErrorEnvelope",
        ),
    ],
};

/// `GET /v1/channel-digests/results/{result_id}`.
pub const RESULT_DOC: RouteDoc = RouteDoc {
    method: DocMethod::Get,
    path: RESULT_ROUTE,
    operation_id: "getChannelDigestResult",
    summary: "Read one digest result",
    description: "One of the caller's digest results, with its recap when it has one. A result \
                  that does not exist and one that belongs to someone else are the same 404. The \
                  response is never cached.",
    tag: "channel-digests",
    security: Security::Session,
    parameters: &[Parameter {
        name: "result_id",
        location: In::Path,
        required: true,
        format: Some("uuid"),
        description: "The result's identity, as a lowercase UUID.",
    }],
    request: None,
    responses: &[
        response(200, "The result.", "ChannelDigestResultView"),
        response(
            400,
            "An identifier that is not a result id.",
            "ErrorEnvelope",
        ),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(404, "No such result for this caller.", "ErrorEnvelope"),
        response(
            502,
            "The digest service returned an invalid response.",
            "ErrorEnvelope",
        ),
        response(
            503,
            "The digest service is unavailable or not configured.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "The digest service did not answer in time.",
            "ErrorEnvelope",
        ),
    ],
};
