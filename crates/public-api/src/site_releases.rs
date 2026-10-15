//! `POST /v1/sites/{site}/releases`: the owner asks for a release build of a site (workspace change
//! `add-site-publications`, specs `site-route-classes` and `site-publications`, design D6).
//!
//! The route is the class `release` (ADR-0017). Only Edge publishes commands, so Edge serves it: in
//! one transaction it reserves the `Idempotency-Key`, mints an operation of the kind
//! `sites.release.build`, writes `cmd.sites.release.build_requested.v1` to the outbox and audits.
//! The sites service consumes the command and reports the operation.

use axum::Json;
use axum::response::{IntoResponse as _, Response};
use http::{HeaderMap, StatusCode};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_eventing::{MessageClass, PLATFORM_PRODUCER, Subject};
use ratatoskr_event_envelope::{
    CommandEnvelope, CommandPayload, EnvelopeSchemaVersion, ProducerName,
};
use ratatoskr_identifiers::{
    CommandId, EntityRef, Extensions, OperationId, TenantRef, UserId, WireTimestamp,
};
use ratatoskr_sites_contracts::{ReleaseBuildRequested, SiteId};
use uuid::Uuid;

use crate::intake::{Intake, Prepared, accept};
use crate::{ApiState, Principal};

/// The kind of the operation that a build request mints.
const BUILD_KIND: &str = "sites.release.build";

/// The route string that the idempotency ledger stores: plain segments, because the ledger refuses
/// a template. The site in the path is the fingerprint instead.
const BUILD_LEDGER_ROUTE: &str = "/v1/sites/releases";

/// The header of the caller's key.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// What a client gets back from an accepted build request.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "SiteReleaseBuildAccepted")]
pub struct BuildAccepted {
    /// The operation to poll at `/v1/operations/{id}`.
    pub operation_id: Uuid,
    /// Always `accepted` here.
    pub status: &'static str,
}

fn accepted(operation_id: Uuid) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(BuildAccepted {
            operation_id,
            status: "accepted",
        }),
    )
        .into_response()
}

/// `POST /v1/sites/{site}/releases`. The caller passed the guard of the class `release`.
pub(crate) async fn request_build(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
    headers: &HeaderMap,
) -> Response {
    let Some(key) = headers
        .get(IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 255)
    else {
        return platform_http::reject(FailureKind::MissingIdempotencyKey);
    };
    let Ok(site) = SiteId::parse(site) else {
        return platform_http::reject(FailureKind::NotFound);
    };
    let intake = Intake {
        route: BUILD_LEDGER_ROUTE,
        kind: BUILD_KIND,
        audit_action: BUILD_KIND,
        key,
        fingerprint: site.as_str().as_bytes(),
        correlation,
        reporter: None,
    };
    accept(state, principal, &intake, accepted, |operation, now| {
        command(&site, operation.operation_id, principal.user_id, now)
    })
    .await
}

/// The `CommandEnvelope` of a build: aggregate `site:<id>`, correlation `operation:<id>`, tenant
/// the owner.
fn command(
    site: &SiteId,
    operation_id: Uuid,
    user: Uuid,
    now: jiff::Timestamp,
) -> Result<Prepared, FailureKind> {
    fn internal<E>(_: E) -> FailureKind {
        tracing::error!("a site build command could not be built");
        FailureKind::RequestTimeout
    }
    let payload = ReleaseBuildRequested {
        operation_id: OperationId(operation_id),
        site_id: site.clone(),
    };
    let serde_json::Value::Object(payload) = serde_json::to_value(payload).map_err(internal)?
    else {
        return Err(FailureKind::RequestTimeout);
    };
    let command_id = Uuid::now_v7();
    let envelope = CommandEnvelope {
        command_id: CommandId(command_id),
        command_type: ReleaseBuildRequested::command_type(),
        issued_at: WireTimestamp::from_jiff(now),
        producer: ProducerName::parse(PLATFORM_PRODUCER).map_err(internal)?,
        aggregate_id: EntityRef::parse(&format!("site:{}", site.as_str())).map_err(internal)?,
        correlation_id: OperationId(operation_id).as_entity_ref(),
        causation_id: None,
        tenant_id: Some(TenantRef::of_user(UserId(user))),
        schema_version: EnvelopeSchemaVersion::CURRENT,
        payload,
        extensions: Extensions::new(),
    };
    Ok(Prepared {
        subject: Subject::new(MessageClass::Command, ReleaseBuildRequested::COMMAND_TYPE)
            .map_err(internal)?,
        command_id,
        payload: serde_json::to_value(envelope).map_err(internal)?,
    })
}

/// How the build route is described in the generated `OpenAPI` document.
///
/// The route is not in the route table of [`crate::routes`]: Edge serves it only when the
/// deployment routes the sites service.
pub const BUILD_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sites/{site}/releases",
    operation_id: "requestSiteReleaseBuild",
    summary: "Request a release build of a site",
    description: "\
Accepts a request to build a release of the site durably and returns the operation of kind \
`sites.release.build` that tracks it; the sites service builds the release and reports the outcome \
on the operation. A build that makes no new release succeeds and names the current release.\n\n\
The first release that holds a publication is always requested here, whatever the release mode of \
the publication. `Idempotency-Key` is required: the same key for the same site returns the \
ORIGINAL operation. Only a primary session whose user holds the live `platform.owner` grant may \
call this route (class `release`, ADR-0017).",
    tag: "sites",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "site",
            location: In::Path,
            required: true,
            format: None,
            description: "The site id: 3 to 40 lower-case ASCII letters, digits and `-`.",
        },
        Parameter {
            name: "Idempotency-Key",
            location: In::Header,
            required: true,
            format: None,
            description: "A client-chosen key, 1 to 255 characters, unique per distinct request. \
                          It is hashed before it is stored.",
        },
    ],
    request: None,
    responses: &[
        ResponseDoc {
            status: 202,
            description: "Accepted durably. The body carries the operation to poll.",
            payload: Some(Payload::Json("SiteReleaseBuildAccepted")),
        },
        ResponseDoc {
            status: 400,
            description: "No `Idempotency-Key`.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 403,
            description: "The session is not a primary session whose user holds the live \
                          `platform.owner` grant.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The site id is not a valid site id.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The key is in use for a different request, or an earlier attempt with \
                          it has not finished.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was written; retrying with \
                          the same key is safe.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
