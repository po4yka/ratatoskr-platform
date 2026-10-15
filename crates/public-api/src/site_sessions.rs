//! The site sign-in and the site owner session at Edge (workspace change
//! `add-site-owner-editing-and-live`, design D1, D2 and D4; spec `site-owner-session`).
//!
//! The owner's primary session asks for a sign-in code (`POST .../sign-in-codes`, a management
//! route); the site's callback page redeems it with its PKCE verifier (`POST .../sessions`, the one
//! `sign-in` route) and gets the host-only cookie `__Host-ratatoskr-site`. Edge reads that cookie only
//! on the routes of its own site that take a site owner session, at the audience `site:<site id>`,
//! and an unsafe method there needs `X-Ratatoskr-Site-Request: 1` and the `Origin` of the approved
//! hostname.

use std::sync::Arc;

use axum::body::Body;
use axum::response::{IntoResponse as _, Response};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use platform_core::FailureKind;
use platform_http::RequestContext;
use platform_identity::session::{self, site_audience};
use platform_identity::site_sign_in::{self, NewSignInCode, SignInRedeem};
use platform_identity::{NewSession, SecretDigest, SessionKind};
use ratatoskr_error_contracts::ErrorEnvelope;
use ratatoskr_sites_contracts::SiteId;

use crate::{ApiState, Principal};

/// The cookie of a site owner session. `__Host-` makes the browser keep it host-only, `Secure` and
/// at `Path=/`.
pub(crate) const COOKIE: &str = "__Host-ratatoskr-site";

/// The header that an unsafe request of a site page must carry (design D4).
const SITE_REQUEST_HEADER: &str = "x-ratatoskr-site-request";

/// The largest body of the two routes.
const MAX_BODY: usize = 4096;

/// What the owner's primary session sends to `POST /v1/sites/{site}/sign-in-codes`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignInCodeRequest {
    /// The PKCE S256 challenge of the site's verifier: 43 base64url characters.
    pub challenge: String,
    /// The path on the site to return to: it starts with one `/`, at most 1024 bytes.
    pub return_path: String,
}

/// The answer of `POST /v1/sites/{site}/sign-in-codes`.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SignInCodeIssued {
    /// The single-use code. It is shown once and expires after 60 seconds.
    pub code: String,
    /// The SDK callback page of the approved hostname, with the code.
    pub redirect_url: String,
    /// When the code expires.
    pub expires_at: String,
}

/// What the site's callback page sends to `POST /v1/sites/{site}/sessions`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteSessionRequest {
    /// The code from the redirect.
    pub code: String,
    /// The PKCE verifier that the site kept.
    pub verifier: String,
}

/// The answer of `POST /v1/sites/{site}/sessions`. The credential is only in the cookie.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SiteSessionOpened {
    /// The path on the site to return to.
    pub return_path: String,
    /// When the session ends at the latest.
    pub expires_at: String,
}

/// The 404 `sites.site_not_found` that the sites service answers for a site it does not serve.
/// Edge gives the same answer for an unapproved site, so that an answer never shows whether a
/// site exists (workspace spec `site-capability-enforcement`). The body is the sites service's
/// contract envelope, validated as a relayed answer is.
pub(crate) fn site_not_found(context: &RequestContext) -> Response {
    let body = serde_json::json!({
        "code": "sites.site_not_found",
        "message": "No such site.",
        "retryable": false,
        "correlation_id": context.correlation_id.to_string(),
    });
    match serde_json::from_value::<ErrorEnvelope>(body) {
        Ok(envelope) => platform_http::preserve_contract_error(
            (StatusCode::NOT_FOUND, axum::Json(envelope)).into_response(),
        ),
        Err(_) => platform_http::reject(FailureKind::NotFound),
    }
}

/// The hostname of the approval of `site`, when the site is approved.
pub(crate) async fn approved_hostname(
    state: &ApiState,
    site: &str,
) -> Result<Option<String>, FailureKind> {
    sqlx::query_scalar(
        "select hostname from identity.site_approvals where site_id = $1 and state = 'approved'",
    )
    .bind(site)
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| {
        tracing::error!(%error, "the approval of a site could not be read");
        FailureKind::RequestTimeout
    })
}

/// Whether `path` is a path on the site that a redirect may lead to: one leading `/`, no second
/// `/` or `\` after it, no control character, at most 1024 bytes.
fn is_return_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.starts_with("/\\")
        && path.len() <= 1024
        && !path.chars().any(char::is_control)
}

/// `POST /v1/sites/{site}/sign-in-codes`: the owner's primary session approves a sign-in. The
/// caller has passed the management guard.
pub(crate) async fn issue_code(
    state: &Arc<ApiState>,
    principal: Principal,
    context: &RequestContext,
    site: &str,
    body: Body,
) -> Response {
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    let Ok(request) = serde_json::from_slice::<SignInCodeRequest>(&bytes) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    let challenge_valid = request.challenge.len() == 43
        && request
            .challenge
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !challenge_valid || !is_return_path(&request.return_path) {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    if SiteId::parse(site).is_err() {
        return site_not_found(context);
    }
    let hostname = match approved_hostname(state, site).await {
        Ok(Some(hostname)) => hostname,
        Ok(None) => return site_not_found(context),
        Err(refusal) => return platform_http::reject(refusal),
    };
    let now = jiff::Timestamp::now();
    let issued = async {
        let mut transaction = state.database.pool().begin().await.ok()?;
        let code = site_sign_in::issue(
            &mut transaction,
            &NewSignInCode {
                user_id: principal.user_id,
                created_by_session_id: principal.session_id,
                site_id: site,
                challenge: &request.challenge,
                return_path: &request.return_path,
                hostname: &hostname,
                now,
            },
        )
        .await
        .ok()?;
        transaction.commit().await.ok()?;
        Some(code)
    }
    .await;
    let code = match issued {
        Some(Ok(code)) => code,
        Some(Err(_)) => return platform_http::reject(FailureKind::Forbidden),
        None => {
            tracing::error!("a site sign-in code could not be stored");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let issued = SignInCodeIssued {
        redirect_url: format!("https://{hostname}/.ratatoskr/sign-in/?code={code}"),
        code,
        expires_at: (now + site_sign_in::CODE_TTL).to_string(),
    };
    let mut response = (StatusCode::CREATED, axum::Json(issued)).into_response();
    no_store(response.headers_mut());
    response
}

/// `POST /v1/sites/{site}/sessions`: the site's callback page redeems a code with its verifier,
/// from the approved hostname, and gets a `site_owner` session in the cookie. Every refusal is the
/// stable unauthenticated error.
pub(crate) async fn open_session(
    state: &Arc<ApiState>,
    site: &str,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let refuse = || platform_http::reject(FailureKind::Unauthenticated);
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    let Ok(request) = serde_json::from_slice::<SiteSessionRequest>(&bytes) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    if request.code.is_empty() || request.code.len() > 128 || request.verifier.len() > 128 {
        return refuse();
    }
    let now = jiff::Timestamp::now();
    let Ok((credential, digest)) = session::mint_credential() else {
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    let audience = site_audience(site);
    let opened = async {
        let mut transaction = state.database.pool().begin().await.ok()?;
        let redeemed = site_sign_in::redeem(
            &mut transaction,
            &SignInRedeem {
                site_id: site,
                code: &request.code,
                verifier: &request.verifier,
                now,
            },
        )
        .await
        .ok()?;
        // The code is consumed by this presentation even when it is refused below.
        let accepted = match redeemed {
            Ok(redeemed)
                if origin_is(headers, &redeemed.hostname)
                    && approved_now(&mut transaction, site, &redeemed.hostname).await? =>
            {
                session::create_session(
                    &mut *transaction,
                    &NewSession::site_owner(redeemed.user_id, &audience, digest, now),
                )
                .await
                .ok()?;
                Some(redeemed.return_path)
            }
            _ => None,
        };
        transaction.commit().await.ok()?;
        Some(accepted)
    }
    .await;
    let return_path = match opened {
        Some(Some(return_path)) => return_path,
        Some(None) => return refuse(),
        None => {
            tracing::error!("a site sign-in could not be completed");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    };
    let opened = SiteSessionOpened {
        return_path,
        expires_at: (now + session::SITE_OWNER_LIFETIME).to_string(),
    };
    let mut response = (StatusCode::CREATED, axum::Json(opened)).into_response();
    let cookie = format!(
        "{COOKIE}={credential}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={}",
        session::SITE_OWNER_LIFETIME.as_secs()
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .insert(http::header::SET_COOKIE, value);
    }
    no_store(response.headers_mut());
    response
}

/// Whether the site's approval still names `hostname`. `None` when the read fails.
async fn approved_now(
    transaction: &mut sqlx::PgTransaction<'_>,
    site: &str,
    hostname: &str,
) -> Option<bool> {
    sqlx::query_scalar::<_, bool>(
        "select exists (select 1 from identity.site_approvals
                         where site_id = $1 and state = 'approved' and hostname = $2)",
    )
    .bind(site)
    .bind(hostname)
    .fetch_one(&mut **transaction)
    .await
    .ok()
}

/// Whether the request's `Origin` is `https://` and `hostname`.
fn origin_is(headers: &HeaderMap, hostname: &str) -> bool {
    headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin.strip_prefix("https://") == Some(hostname))
}

fn no_store(headers: &mut HeaderMap) {
    headers.insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    headers.insert(
        http::header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

/// The value of the site cookie of a request, when it carries one.
fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE).then_some(value)
        })
}

/// The `site_owner` session of `site` in the cookie of a request, or `None` when the request
/// carries no cookie or one that does not authenticate at the audience of `site`. Edge reads the
/// cookie only here, on the routes of `site` that take a site owner session.
///
/// An unsafe method of a site owner session also needs the site request header and the `Origin`
/// of the approved hostname, or it is refused with the stable forbidden error (design D4).
///
/// # Errors
///
/// The refusal response: forbidden, rate limited, or a timeout when the database cannot answer.
pub(crate) async fn site_owner(
    state: &Arc<ApiState>,
    parts: &http::request::Parts,
    site: &str,
) -> Result<Option<Principal>, Response> {
    let Some(token) = cookie(&parts.headers) else {
        return Ok(None);
    };
    let now = jiff::Timestamp::now();
    let found = session::authenticate(
        state.database.pool(),
        SecretDigest::of(token),
        &site_audience(site),
        now,
    )
    .await
    .map_err(|error| {
        tracing::error!(%error, "a site session could not be authenticated");
        platform_http::reject(FailureKind::RequestTimeout)
    })?;
    let Some(found) = found.filter(|found| found.kind == SessionKind::SiteOwner) else {
        return Ok(None);
    };
    if !state.actor_limit.admit(found.user_id, now) {
        return Err(platform_http::reject(FailureKind::RateLimited));
    }
    if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
        let hostname = approved_hostname(state, site)
            .await
            .map_err(platform_http::reject)?;
        let marked = parts
            .headers
            .get(SITE_REQUEST_HEADER)
            .is_some_and(|value| value.as_bytes() == b"1");
        if !marked || !hostname.is_some_and(|hostname| origin_is(&parts.headers, &hostname)) {
            return Err(platform_http::reject(FailureKind::Forbidden));
        }
    }
    if let Err(error) = session::touch_last_seen(
        state.database.pool(),
        found.session_id,
        None,
        now,
        jiff::SignedDuration::from_secs(60),
    )
    .await
    {
        tracing::debug!(%error, "the liveness touch of a site session could not be written");
    }
    Ok(Some(Principal {
        user_id: found.user_id,
        session_id: found.session_id,
        device_id: None,
        kind: SessionKind::SiteOwner,
    }))
}

/// What `POST /v1/sessions/revoke-site` takes.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeSiteSessions {
    /// The site whose owner sessions end.
    pub site_id: String,
}

/// What `POST /v1/sessions/revoke-site` answers.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct RevokedSiteSessions {
    /// How many live `site_owner` sessions of the site ended.
    pub revoked: u64,
}

/// `POST /v1/sessions/revoke-site`: ends every `site_owner` session of the caller for one site, and
/// records why for each. The sessions of other sites and every other session stay.
pub async fn revoke_site(
    axum::extract::State(state): axum::extract::State<Arc<ApiState>>,
    principal: Principal,
    body: axum::body::Bytes,
) -> Response {
    let Ok(request) = serde_json::from_slice::<RevokeSiteSessions>(&body) else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    if SiteId::parse(&request.site_id).is_err() {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    let now = jiff::Timestamp::now();
    let revoked = async {
        let mut transaction = state.database.pool().begin().await.ok()?;
        let revoked = session::revoke_site_sessions(
            &mut *transaction,
            Some(principal.user_id),
            &request.site_id,
            now,
        )
        .await
        .ok()?;
        for session_id in &revoked {
            platform_identity::record_revocation(
                &mut *transaction,
                platform_identity::RevocationSubject::Session,
                *session_id,
                platform_identity::RevocationReason::UserRequest,
                Some(principal.user_id),
                now,
            )
            .await
            .ok()?;
        }
        transaction.commit().await.ok()?;
        Some(revoked.len())
    }
    .await;
    let Some(revoked) = revoked else {
        tracing::error!("the site sessions could not be revoked");
        return platform_http::reject(FailureKind::RequestTimeout);
    };
    (
        StatusCode::OK,
        axum::Json(RevokedSiteSessions {
            revoked: u64::try_from(revoked).unwrap_or(u64::MAX),
        }),
    )
        .into_response()
}

/// How `POST /v1/sessions/revoke-site` is described in the generated document.
pub const REVOKE_SITE_DOC: platform_api_doc::RouteDoc = platform_api_doc::RouteDoc {
    method: platform_api_doc::Method::Post,
    path: "/v1/sessions/revoke-site",
    operation_id: "revokeSiteSessions",
    summary: "Revoke your sessions on one site",
    description: "\
Ends every `site_owner` session of yours for one site at once. The sessions of other sites and \
every other session stay. The session list shows each `site_owner` session with its site and the \
approved hostname.",
    tag: "sessions",
    security: platform_api_doc::Security::Session,
    parameters: &[],
    request: Some(platform_api_doc::Payload::Json("RevokeSiteSessions")),
    responses: &[
        platform_api_doc::ResponseDoc {
            status: 200,
            description: "The live sessions of the site are revoked; the answer counts them.",
            payload: Some(platform_api_doc::Payload::Json("RevokedSiteSessions")),
        },
        platform_api_doc::ResponseDoc {
            status: 400,
            description: "The body is not a site id.",
            payload: Some(platform_api_doc::Payload::Json("ErrorEnvelope")),
        },
        platform_api_doc::ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate.",
            payload: Some(platform_api_doc::Payload::Json("ErrorEnvelope")),
        },
        platform_api_doc::ResponseDoc {
            status: 504,
            description: "A dependency did not answer in time. Nothing was changed.",
            payload: Some(platform_api_doc::Payload::Json("ErrorEnvelope")),
        },
    ],
};
