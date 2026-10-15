//! `POST` and `DELETE /v1/sites/{site}/manifest/approval`: the owner approves or withdraws the
//! manifest of a site (workspace change `add-site-publications`, spec `site-manifest`, design D9).
//!
//! The approval is an authorisation decision, so Platform holds it: `identity.site_approvals` has
//! one row for each site, bound to the approved manifest digest. Edge reads the active manifest of
//! the site from the loopback route `GET /internal/sites/{site}/manifest` of the sites service,
//! refuses a digest that is not the active one, and writes the row and
//! `evt.platform.site_approval.changed.v1` in one transaction. The sites service keeps only a
//! projection of that event.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse as _, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_eventing::{MessageClass, Outbox, PLATFORM_PRODUCER, Subject};
use platform_identity::audit::{self, AuditEvent, AuditOutcome};
use ratatoskr_event_envelope::{
    EnvelopeSchemaVersion, EventEnvelope, EventPayload as _, ProducerName,
};
use ratatoskr_identifiers::{
    DigestHex, EntityRef, EventId, Extensions, TenantRef, UserId, WireTimestamp,
};
use ratatoskr_sites_contracts::{
    ApprovalState, Hostname, PublicationId, SiteApprovalChanged, SiteCapability, SiteId,
};
use sqlx::Row as _;
use uuid::Uuid;

use crate::gateway::{self, Claims};
use crate::{ApiState, Principal};

/// The largest approval body Edge reads: one digest.
const MAX_BODY_BYTES: usize = 1024;

/// The largest answer of the internal manifest route that Edge reads.
const MAX_MANIFEST_BYTES: usize = 65_536;

/// The unique index that keeps a hostname to one approved site.
const HOSTNAME_INDEX: &str = "site_approvals_hostname_is_unique_while_approved";

/// The body of an approval: the digest of the manifest that the owner read.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "SiteApprovalRequest")]
pub struct ApproveManifest {
    /// The lowercase hex SHA-256 of the manifest bytes that the owner approves.
    pub manifest_sha256: String,
}

/// The approval of one site, as Platform stores it.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "SiteApproval")]
pub struct SiteApproval {
    /// The site.
    pub site_id: String,
    /// The approved manifest digest.
    pub manifest_sha256: String,
    /// The capabilities of the approved manifest.
    pub capabilities: Vec<String>,
    /// The hostname of the approved manifest.
    pub hostname: String,
    /// `approved` or `withdrawn`.
    pub state: String,
}

/// The answer of `GET /internal/sites/{site}/manifest` on the sites listener: the active manifest.
#[derive(Debug, serde::Deserialize)]
struct ActiveManifest {
    site_id: SiteId,
    manifest_sha256: DigestHex,
    hostname: Hostname,
    #[allow(
        dead_code,
        reason = "read so that a manifest without publications is refused"
    )]
    publications: Vec<PublicationId>,
    capabilities: Vec<SiteCapability>,
}

/// The stored row of a site, read under a lock in the transaction that changes it.
struct Stored {
    manifest_sha256: String,
    capabilities: Vec<String>,
    hostname: String,
    state: String,
}

impl Stored {
    fn answer(self, site: &str) -> Response {
        Json(SiteApproval {
            site_id: site.to_owned(),
            manifest_sha256: self.manifest_sha256,
            capabilities: self.capabilities,
            hostname: self.hostname,
            state: self.state,
        })
        .into_response()
    }
}

/// `POST /v1/sites/{site}/manifest/approval`. The caller passed the `management` guard.
pub(crate) async fn approve(
    state: &Arc<ApiState>,
    principal: Principal,
    correlation: &str,
    site: &str,
    body: Body,
) -> Response {
    let requested = match read_body(body).await {
        Ok(requested) => requested,
        Err(refusal) => return platform_http::reject(refusal),
    };
    let active = match active_manifest(state, principal, correlation, site).await {
        Ok(active) => active,
        Err(refusal) => return platform_http::reject(refusal),
    };
    if active.site_id.as_str() != site {
        tracing::warn!("the sites service answered the manifest of another site");
        return platform_http::reject(FailureKind::UpstreamInvalidResponse);
    }
    if active.manifest_sha256.as_str() != requested.as_str() {
        return platform_http::reject(FailureKind::ManifestDigestStale);
    }
    let change = SiteApprovalChanged {
        site_id: active.site_id,
        manifest_sha256: active.manifest_sha256,
        capabilities: active.capabilities,
        hostname: active.hostname,
        state: ApprovalState::Approved,
    };
    match store(state, principal, correlation, &change).await {
        Ok(response) => response,
        Err(refusal) => platform_http::reject(refusal),
    }
}

/// `DELETE /v1/sites/{site}/manifest/approval`. The caller passed the `management` guard.
pub(crate) async fn withdraw(
    state: &Arc<ApiState>,
    principal: Principal,
    correlation: &str,
    site: &str,
) -> Response {
    match withdraw_stored(state, principal, correlation, site).await {
        Ok(response) => response,
        Err(refusal) => platform_http::reject(refusal),
    }
}

/// The digest of an approval body.
async fn read_body(body: Body) -> Result<DigestHex, FailureKind> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| FailureKind::InvalidRequest)?;
    let request: ApproveManifest =
        serde_json::from_slice(&bytes).map_err(|_| FailureKind::InvalidRequest)?;
    DigestHex::parse(&request.manifest_sha256).map_err(|_| FailureKind::InvalidRequest)
}

/// The active manifest of `site`, read from the loopback route of the sites service.
async fn active_manifest(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
) -> Result<ActiveManifest, FailureKind> {
    let listener = state
        .gateway
        .service_listener(crate::sites::SERVICE)
        .ok_or(FailureKind::UpstreamUnavailable)?;
    let site = SiteId::parse(site).map_err(|_| FailureKind::NotFound)?;
    let mut request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!(
            "http://{listener}/internal/sites/{}/manifest",
            site.as_str()
        ))
        .body(Body::empty())
        .map_err(|_| FailureKind::UpstreamUnavailable)?;
    *request.headers_mut() = gateway::claim_headers(Claims {
        principal,
        device_kind: None,
        site_id: None,
        correlation_id: correlation,
    });
    state
        .gateway
        .fetch_json("sites", request, true, MAX_MANIFEST_BYTES)
        .await
}

/// A failed statement of the approval transaction.
fn failed(error: &sqlx::Error) -> FailureKind {
    tracing::error!(%error, "a site approval could not be written");
    FailureKind::RequestTimeout
}

/// The row of `site`, locked until the transaction ends.
async fn locked(
    transaction: &mut sqlx::PgTransaction<'_>,
    site: &str,
) -> Result<Option<Stored>, FailureKind> {
    let row = sqlx::query(
        "select manifest_sha256, capabilities, hostname, state
           from identity.site_approvals where site_id = $1 for update",
    )
    .bind(site)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| failed(&error))?;
    Ok(row.map(|row| Stored {
        manifest_sha256: row.get("manifest_sha256"),
        capabilities: row.get("capabilities"),
        hostname: row.get("hostname"),
        state: row.get("state"),
    }))
}

/// Write the approval of `change` and announce it, unless the site already has that approval.
async fn store(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    change: &SiteApprovalChanged,
) -> Result<Response, FailureKind> {
    let site = change.site_id.as_str();
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| failed(&error))?;
    let stored = locked(&mut transaction, site).await?;
    if let Some(stored) = stored.filter(|stored| {
        stored.state == "approved" && stored.manifest_sha256 == change.manifest_sha256.as_str()
    }) {
        return Ok(stored.answer(site));
    }
    let capabilities: Vec<&str> = change
        .capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect();
    let now = jiff::Timestamp::now();
    let written = sqlx::query(
        "insert into identity.site_approvals
             (site_id, changed_by, manifest_sha256, capabilities, hostname, state, changed_at)
         values ($1, $2, $3, $4, $5, 'approved', $6)
         on conflict (site_id) do update
            set changed_by = excluded.changed_by, manifest_sha256 = excluded.manifest_sha256,
                capabilities = excluded.capabilities, hostname = excluded.hostname,
                state = 'approved', changed_at = excluded.changed_at",
    )
    .bind(site)
    .bind(principal.user_id)
    .bind(change.manifest_sha256.as_str())
    .bind(&capabilities)
    .bind(change.hostname.as_str())
    .bind(to_offset(now))
    .execute(&mut *transaction)
    .await;
    match written {
        Err(sqlx::Error::Database(error)) if error.constraint() == Some(HOSTNAME_INDEX) => {
            return Err(FailureKind::SiteHostnameTaken);
        }
        Err(error) => return Err(failed(&error)),
        Ok(_) => {}
    }
    announce(&mut transaction, principal, correlation, change, now).await?;
    transaction.commit().await.map_err(|error| failed(&error))?;
    Ok(Stored {
        manifest_sha256: change.manifest_sha256.as_str().to_owned(),
        capabilities: capabilities.into_iter().map(str::to_owned).collect(),
        hostname: change.hostname.as_str().to_owned(),
        state: "approved".to_owned(),
    }
    .answer(site))
}

/// Mark the approval of `site` withdrawn and announce it, unless it is withdrawn already. A site
/// without an approval is `404`.
async fn withdraw_stored(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
) -> Result<Response, FailureKind> {
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| failed(&error))?;
    let Some(mut stored) = locked(&mut transaction, site).await? else {
        return Err(FailureKind::NotFound);
    };
    if stored.state == "withdrawn" {
        return Ok(stored.answer(site));
    }
    let change = withdrawal(site, &stored)?;
    let now = jiff::Timestamp::now();
    sqlx::query(
        "update identity.site_approvals
            set state = 'withdrawn', changed_by = $2, changed_at = $3
          where site_id = $1",
    )
    .bind(site)
    .bind(principal.user_id)
    .bind(to_offset(now))
    .execute(&mut *transaction)
    .await
    .map_err(|error| failed(&error))?;
    announce(&mut transaction, principal, correlation, &change, now).await?;
    transaction.commit().await.map_err(|error| failed(&error))?;
    "withdrawn".clone_into(&mut stored.state);
    Ok(stored.answer(site))
}

/// The withdrawal event of a stored approval: it repeats the digest, the capabilities and the
/// hostname of the approval it ends.
fn withdrawal(site: &str, stored: &Stored) -> Result<SiteApprovalChanged, FailureKind> {
    fn corrupt<E>(_: E) -> FailureKind {
        tracing::error!("a stored site approval does not parse");
        FailureKind::RequestTimeout
    }
    let capabilities = stored
        .capabilities
        .iter()
        .map(|capability| serde_json::from_value(serde_json::Value::from(capability.as_str())))
        .collect::<Result<Vec<SiteCapability>, _>>()
        .map_err(corrupt)?;
    Ok(SiteApprovalChanged {
        site_id: SiteId::parse(site).map_err(corrupt)?,
        manifest_sha256: DigestHex::parse(&stored.manifest_sha256).map_err(corrupt)?,
        capabilities,
        hostname: Hostname::parse(&stored.hostname).map_err(corrupt)?,
        state: ApprovalState::Withdrawn,
    })
}

/// Write `evt.platform.site_approval.changed.v1` to the outbox and the audit record, in the
/// transaction of the change. The envelope names the site as `site:<id>`, the owner as the tenant
/// and the request correlation.
async fn announce(
    transaction: &mut sqlx::PgTransaction<'_>,
    principal: Principal,
    correlation: &str,
    change: &SiteApprovalChanged,
    now: jiff::Timestamp,
) -> Result<(), FailureKind> {
    fn internal<E>(_: E) -> FailureKind {
        tracing::error!("a site approval event could not be built");
        FailureKind::RequestTimeout
    }
    let serde_json::Value::Object(payload) = serde_json::to_value(change).map_err(internal)? else {
        return Err(FailureKind::RequestTimeout);
    };
    let event_id = Uuid::now_v7();
    let envelope = EventEnvelope {
        event_id: EventId(event_id),
        event_type: SiteApprovalChanged::event_type(),
        occurred_at: WireTimestamp::from_jiff(now),
        producer: ProducerName::parse(PLATFORM_PRODUCER).map_err(internal)?,
        aggregate_id: EntityRef::parse(&format!("site:{}", change.site_id.as_str()))
            .map_err(internal)?,
        correlation_id: EntityRef::parse(correlation).map_err(internal)?,
        causation_id: None,
        tenant_id: Some(TenantRef::of_user(UserId(principal.user_id))),
        schema_version: EnvelopeSchemaVersion::CURRENT,
        payload,
        extensions: Extensions::new(),
    };
    let subject =
        Subject::new(MessageClass::Event, SiteApprovalChanged::EVENT_TYPE).map_err(internal)?;
    let envelope = serde_json::to_value(envelope).map_err(internal)?;
    Outbox::enqueue(&mut **transaction, event_id, &subject, &envelope, None, now)
        .await
        .map_err(|error| {
            tracing::error!(%error, "a site approval event could not be enqueued");
            FailureKind::RequestTimeout
        })?;
    let action = match change.state {
        ApprovalState::Withdrawn => "site.approval.withdraw",
        _ => "site.approval.approve",
    };
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action,
        target_kind: "site",
        target_id: None,
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.to_owned(),
    };
    audit::record(&mut **transaction, &event, now)
        .await
        .map_err(|error| {
            tracing::error!(%error, "a site approval could not be audited");
            FailureKind::RequestTimeout
        })?;
    Ok(())
}

fn to_offset(instant: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(instant.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

/// The site in the path of the approval routes.
const SITE_PARAMETER: Parameter = Parameter {
    name: "site",
    location: In::Path,
    required: true,
    format: None,
    description: "The site id: 3 to 40 lower-case ASCII letters, digits and `-`.",
};

/// One documented answer.
const fn response(status: u16, description: &'static str, payload: &'static str) -> ResponseDoc {
    ResponseDoc {
        status,
        description,
        payload: Some(Payload::Json(payload)),
    }
}

/// The refusals of the `management` class that both routes share.
const GUARD_401: ResponseDoc = response(
    401,
    "No credential, or one that does not authenticate here.",
    "ErrorEnvelope",
);
const GUARD_403: ResponseDoc = response(
    403,
    "The session is not a primary session whose user holds the live `platform.owner` grant.",
    "ErrorEnvelope",
);

/// How the approval route is described in the generated `OpenAPI` document.
///
/// The route is not in the route table of [`crate::routes`]: Edge serves it only when the
/// deployment routes the sites service.
pub const APPROVE_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sites/{site}/manifest/approval",
    operation_id: "approveSiteManifest",
    summary: "Approve the active manifest of a site",
    description: "\
Approves the manifest that the sites service holds as the active manifest of the site. The body \
names the SHA-256 of the manifest bytes that the owner read; Edge reads the active manifest from \
the sites service and refuses a digest that is not the active one with \
`platform.site.manifest_digest_stale`. A hostname belongs to one approved site: an approval that would \
give a hostname to a second site is refused with `platform.site.hostname_taken`.\n\n\
Platform stores the approval and announces it to the sites service in the same transaction. An \
approval of the digest that is approved already changes nothing and announces nothing. Only a \
primary session whose user holds the live `platform.owner` grant may call this route (class \
`management`, ADR-0017).",
    tag: "sites",
    security: Security::Session,
    parameters: &[SITE_PARAMETER],
    request: Some(Payload::Json("SiteApprovalRequest")),
    responses: &[
        response(200, "The approval of the site.", "SiteApproval"),
        response(
            400,
            "The body is not one `manifest_sha256` of 64 lower-case hex digits.",
            "ErrorEnvelope",
        ),
        GUARD_401,
        GUARD_403,
        response(
            404,
            "The sites service holds no manifest for the site.",
            "ErrorEnvelope",
        ),
        response(
            409,
            "`platform.site.manifest_digest_stale`: the digest is not the active manifest. \
             `platform.site.hostname_taken`: another approved site has the hostname.",
            "ErrorEnvelope",
        ),
        response(
            502,
            "The sites service answered something that is not an active manifest.",
            "ErrorEnvelope",
        ),
        response(
            503,
            "The sites service could not be reached.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "The sites service did not answer in time.",
            "ErrorEnvelope",
        ),
    ],
};

/// How the withdrawal route is described in the generated `OpenAPI` document.
pub const WITHDRAW_DOC: RouteDoc = RouteDoc {
    method: Method::Delete,
    path: "/v1/sites/{site}/manifest/approval",
    operation_id: "withdrawSiteManifest",
    summary: "Withdraw the approval of a site",
    description: "\
Withdraws the approval of the site. The site then serves nothing. Platform stores the state \
`withdrawn` and announces it to the sites service with the digest, the capabilities and the \
hostname of the approval it ends. A withdrawal of a withdrawn approval changes nothing and \
announces nothing. Only a primary session whose user holds the live `platform.owner` grant may \
call this route (class `management`, ADR-0017).",
    tag: "sites",
    security: Security::Session,
    parameters: &[SITE_PARAMETER],
    request: None,
    responses: &[
        response(
            200,
            "The approval of the site, now withdrawn.",
            "SiteApproval",
        ),
        GUARD_401,
        GUARD_403,
        response(404, "The site has no approval.", "ErrorEnvelope"),
    ],
};
