//! The provider callback facade.
//!
//! `ARCHITECTURE.md` S6.4 gives Platform one job and withholds every other: it may host the public
//! callback route, and the owning provider service generates or validates the state, exchanges the
//! code, stores the tokens and records the scopes. ADR-0012 fixes the mechanism — a one-time,
//! audience-bound row, claimed once by the service that owns the provider.
//!
//! The callback route is unauthenticated and **cannot** be otherwise: a provider redirects a browser
//! to it, and Platform holds neither the `state` it would need to recognise a real one nor the client
//! secret it would need to judge it. So every value it accepts is attacker-chosen, and it is built on
//! that basis rather than on the provider being honest.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse as _, Redirect, Response};
use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use platform_core::FailureKind;
use platform_identity::{
    AuditEvent, AuditOutcome, CallbackOutcome, IdentityProvider, SessionKind, audit, grant, relay,
};
use uuid::Uuid;

use crate::{ApiState, Principal};

/// How long a relay may go unclaimed.
///
/// Five minutes. Every provider worth integrating expires an authorization code in about that time,
/// so a longer window would keep a value that is already useless; a shorter one would break a service
/// that happens to be restarting when a callback lands.
const RELAY_TTL: jiff::SignedDuration = jiff::SignedDuration::from_mins(5);

/// What a provider puts in the query string.
///
/// Bounded here and again by the schema. `state` is required because `AGENTS.md` makes it mandatory
/// where applicable, and it is applicable to every provider this facade will ever front.
#[derive(Debug, serde::Deserialize)]
pub struct Callback {
    /// The opaque value the owning service issued. Carried verbatim; Platform cannot judge it.
    ///
    /// `Option` even though it is required, so that a missing one is a failure this handler NAMES
    /// rather than one the extractor implies through a status nobody here chose. See [`bounded`].
    pub state: Option<String>,
    /// The authorization code, when the user authorized.
    pub code: Option<String>,
    /// The provider's error code, when they did not.
    pub error: Option<String>,
}

/// What the owning service gets, once.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct RelayedCallback {
    /// Which provider redirected.
    pub provider: String,
    /// The `state` this service issued, so it can match the callback to its own flow.
    pub state: String,
    /// The authorization code. Present exactly once, in this response, and nowhere else.
    pub code: Option<String>,
    /// The provider's error, when the user refused.
    pub error: Option<String>,
}

/// The capability a caller must hold to claim this provider's callbacks.
///
/// A function rather than configuration, for the same reason `provider` is a closed list: a mapping
/// an operator could edit would be a way to address somebody else's callback to a principal of their
/// choosing.
///
/// It is a GRANT and not a session audience, which was the first design and could never have
/// worked: `identity.sessions.audience` names the listener a session may be presented at, so a
/// session claiming to be `ratatoskr-github` would not have authenticated at the edge listener at
/// all. `ARCHITECTURE.md` S7 makes authorization a capability question, and
/// `identity.grants` is where capabilities live.
const fn claim_grant(provider: IdentityProvider) -> &'static str {
    match provider {
        IdentityProvider::Telegram => "oauth.claim.telegram",
        IdentityProvider::GitHub => "oauth.claim.github",
        IdentityProvider::Email => "oauth.claim.email",
    }
}

/// Every claim grant, so the claim route can ask which of them a caller holds without enumerating
/// providers at the call site.
fn every_claim_grant() -> Vec<String> {
    IdentityProvider::ALL
        .into_iter()
        .map(|provider| claim_grant(provider).to_owned())
        .collect()
}

/// The longest value this route will store, per field. The schema repeats them; this refuses earlier.
const MAX_STATE: usize = 512;
const MAX_CODE: usize = 2048;
const MAX_ERROR: usize = 200;

/// `GET /v1/oauth/{provider}/callback`.
///
/// Records the callback and sends the browser to the configured completion page. It does not
/// exchange anything, and it deliberately reports the same page whether the provider returned a code
/// or an error: the person is finished here either way, and the outcome is the owning service's to
/// act on.
pub async fn callback(
    State(state): State<Arc<ApiState>>,
    Path(provider): Path<String>,
    Query(query): Query<Callback>,
    context: Option<axum::Extension<platform_http::RequestContext>>,
) -> Response {
    let correlation = crate::correlation_of(context);

    // A path segment nothing serves is a 404, not a row. This is the whole reason the provider
    // vocabulary is closed rather than configured.
    let Some(provider) = IdentityProvider::from_str_opt(&provider) else {
        return platform_http::reject(FailureKind::NotFound);
    };

    let Some(outcome) = bounded(&query) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    // Present, because `bounded` refused everything else.
    let Some(state_value) = query.state.as_deref() else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };

    let now = jiff::Timestamp::now();
    let relay_id = relay::receive(
        state.database.pool(),
        provider,
        claim_grant(provider),
        state_value,
        outcome,
        now,
        RELAY_TTL,
    )
    .await;
    let relay_id = match relay_id {
        Ok(relay_id) => relay_id,
        Err(error) => {
            tracing::error!(%error, "an OAuth callback could not be recorded");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    // No actor: nobody authenticated, which is exactly the case worth auditing. Not in a transaction
    // because the relay is a single insert — there is no pair to keep together.
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: None,
        actor_session_id: None,
        action: "oauth.callback",
        target_kind: "oauth_relay",
        target_id: Some(relay_id),
        outcome: AuditOutcome::Allowed,
        correlation_id: correlation.clone(),
    };
    if let Err(error) = audit::record(state.database.pool(), &event, now).await {
        tracing::error!(%error, "an OAuth callback could not be audited");
    }

    tracing::info!(%provider, %relay_id, "an OAuth callback was relayed");

    // Configured, never taken from the callback. A redirect target read out of an attacker-supplied
    // parameter is an open redirect, and on this route every parameter is attacker-supplied.
    state.oauth_completion_url.as_ref().map_or_else(
        || (http::StatusCode::OK, "callback received").into_response(),
        |url| Redirect::to(url.as_str()).into_response(),
    )
}

/// `POST /v1/oauth/relays/{relay_id}`.
///
/// The protected internal flow `AGENTS.md` requires for transferring a credential to the owning
/// service. It returns the code once and destroys it in the same statement.
pub async fn claim(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(relay_id): Path<Uuid>,
    context: Option<axum::Extension<platform_http::RequestContext>>,
) -> Response {
    let correlation = crate::correlation_of(context);

    // Only a service may claim. A person's session must not be able to lift an authorization code,
    // and a browser that reached this route has either been tricked or is testing us. The kind is
    // checked as well as the grant: a grant is data an operator writes, and the two together mean a
    // mistake in one is not enough.
    if principal.kind != SessionKind::Service {
        return platform_http::reject(FailureKind::NotFound);
    }

    let now = jiff::Timestamp::now();
    let Ok(held) = held_claim_grants(&state, principal.user_id, now).await else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };

    let claimed = relay::claim(state.database.pool(), relay_id, &held, now).await;

    let claimed = match claimed {
        Ok(claimed) => claimed,
        Err(error) => {
            tracing::error!(%error, "an OAuth relay could not be claimed");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };

    let Some(claimed) = claimed else {
        // Unknown, already claimed, expired, or addressed to another service — one answer, because
        // which relays exist and who they are for is not a caller's business.
        audit_claim(
            &state,
            &correlation,
            relay_id,
            principal,
            AuditOutcome::Denied,
        )
        .await;
        return platform_http::reject(FailureKind::NotFound);
    };

    audit_claim(
        &state,
        &correlation,
        relay_id,
        principal,
        AuditOutcome::Allowed,
    )
    .await;

    (
        http::StatusCode::OK,
        Json(RelayedCallback {
            provider: claimed.provider.as_str().to_owned(),
            state: claimed.state,
            code: claimed.code,
            error: claimed.error,
        }),
    )
        .into_response()
}

/// Which claim capabilities this caller holds.
///
/// `Err(())` is a database failure, already logged; the caller answers 504 rather than reporting a
/// dependency outage as a refused authorization.
async fn held_claim_grants(
    state: &ApiState,
    user_id: Uuid,
    now: jiff::Timestamp,
) -> Result<Vec<String>, ()> {
    let mut held = Vec::new();
    for capability in every_claim_grant() {
        match grant::holds(state.database.pool(), user_id, &capability, now).await {
            Ok(true) => held.push(capability),
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "a grant could not be read");
                return Err(());
            }
        }
    }
    Ok(held)
}

/// Record who took, or tried to take, a credential.
async fn audit_claim(
    state: &ApiState,
    correlation: &str,
    relay_id: Uuid,
    principal: Principal,
    outcome: AuditOutcome,
) {
    let event = AuditEvent {
        audit_event_id: Uuid::now_v7(),
        actor_user_id: Some(principal.user_id),
        actor_session_id: Some(principal.session_id),
        action: "oauth.relay_claim",
        target_kind: "oauth_relay",
        target_id: Some(relay_id),
        outcome,
        correlation_id: correlation.to_owned(),
    };
    if let Err(error) = audit::record(state.database.pool(), &event, jiff::Timestamp::now()).await {
        tracing::error!(%error, "an OAuth relay claim could not be audited");
    }
}

/// The one outcome a callback carries, if it is within bounds.
///
/// `None` refuses the request. A provider that sent neither a code nor an error has told us nothing,
/// and a row recording nothing can only confuse whoever reads it later.
fn bounded(query: &Callback) -> Option<CallbackOutcome<'_>> {
    let state = query.state.as_deref()?;
    if state.is_empty() || state.len() > MAX_STATE {
        return None;
    }
    match (query.code.as_deref(), query.error.as_deref()) {
        (Some(code), None) if !code.is_empty() && code.len() <= MAX_CODE => {
            Some(CallbackOutcome::Code(code))
        }
        (None, Some(error)) if !error.is_empty() && error.len() <= MAX_ERROR => {
            Some(CallbackOutcome::Error(error))
        }
        _ => None,
    }
}

/// How the callback route is described in the generated `OpenAPI` document.
pub const CALLBACK_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/oauth/{provider}/callback",
    operation_id: "receiveOauthCallback",
    summary: "Receive a provider's OAuth redirect",
    description: "\
The public endpoint a provider redirects a browser back to. It records the callback for the service \
that owns the provider and sends the browser to this deployment's completion page.\n\n\
Platform does not exchange the authorization code, does not validate the `state`, and holds no \
client secret — all three belong to the owning service, which is the only party that can tell a real \
callback from a forged one. The code is handed to that service over an authenticated route and \
appears in no command, no log and no redirect.\n\n\
Unauthenticated by construction: a browser arriving from a provider carries no credential of ours. \
The same completion page is returned whether the provider authorized or refused, because the person \
is finished here either way.",
    tag: "oauth",
    security: Security::None,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The provider that redirected. A closed vocabulary; anything else is 404.",
        },
        Parameter {
            name: "state",
            location: In::Query,
            required: true,
            format: None,
            description: "The opaque value the owning service issued, carried back verbatim.",
        },
        Parameter {
            name: "code",
            location: In::Query,
            required: false,
            format: None,
            description: "The authorization code, when the user authorized. Exactly one of `code` \
                          and `error` must be present.",
        },
        Parameter {
            name: "error",
            location: In::Query,
            required: false,
            format: None,
            description: "The provider's error code, when the user refused.",
        },
    ],
    request: None,
    responses: &[
        ResponseDoc {
            status: 303,
            description: "Recorded. The browser is sent to the configured completion page.",
            payload: None,
        },
        ResponseDoc {
            status: 400,
            description: "Neither a `code` nor an `error`, both, or a value outside its bound.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "No such provider.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// How the claim route is described in the generated `OpenAPI` document.
pub const CLAIM_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/oauth/relays/{relay_id}",
    operation_id: "claimOauthRelay",
    summary: "Claim a relayed callback",
    description: "\
For the service that owns the provider, and for nobody else. Returns the authorization code once and \
destroys it in the same statement, so a second claim returns nothing and no copy remains.\n\n\
Requires a SERVICE session whose principal holds the claim capability for that provider, for example \
`oauth.claim.github`. A person's session is refused as if the relay did not exist. So is an unknown \
relay, an expired one, one already claimed, and one whose capability the caller does not hold — one \
answer for all five, because which relays exist and what each needs is not a caller's business.",
    tag: "oauth",
    security: Security::Session,
    parameters: &[Parameter {
        name: "relay_id",
        location: In::Path,
        required: true,
        format: Some("uuid"),
        description: "The relay, as named in the command the owning service received.",
    }],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The callback. Its code appears in this response and nowhere else.",
            payload: Some(Payload::Json("RelayedCallback")),
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
            status: 404,
            description: "No claimable relay of that identity for this caller.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
