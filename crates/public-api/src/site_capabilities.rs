//! The effective capability set of a site at Edge (workspace change
//! `add-site-owner-editing-and-live`, design D3; specs `site-capability-enforcement` and
//! `site-manifest`).
//!
//! The effective set is the capabilities of the last approved manifest
//! (`identity.site_approvals`) that the active manifest still declares (the sites service). Edge
//! checks a capability against it before it forwards a request or reads data for it. When Edge
//! cannot read the set, the check fails closed with the retryable unavailable error.

use platform_core::FailureKind;
use platform_identity::SessionKind;
use ratatoskr_sites_contracts::{SiteCapability, SiteId};
use uuid::Uuid;

use crate::{ApiState, Principal, site_approvals};

/// `notes.edit`: edit the source notes of the site's publications, and request a release.
pub(crate) const NOTES_EDIT: &str = "notes.edit";

/// The effective capability set of `site`, or `None` when the site has no approved manifest.
/// The active manifest is read with the claims of `principal`, or, for an anonymous request, of
/// Edge on behalf of the owner who approved the site.
///
/// # Errors
///
/// [`FailureKind::UpstreamUnavailable`] when the approval or the active manifest cannot be read.
pub(crate) async fn effective_set(
    state: &ApiState,
    principal: Option<Principal>,
    correlation: &str,
    site: &str,
) -> Result<Option<Vec<String>>, FailureKind> {
    let approved: Option<(Vec<String>, Uuid)> = sqlx::query_as(
        "select capabilities, changed_by from identity.site_approvals
          where site_id = $1 and state = 'approved'",
    )
    .bind(site)
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| {
        tracing::error!(%error, "the approval of a site could not be read");
        FailureKind::UpstreamUnavailable
    })?;
    let Some((approved, owner)) = approved else {
        return Ok(None);
    };
    let principal = principal.unwrap_or(Principal {
        user_id: owner,
        session_id: Uuid::nil(),
        device_id: None,
        kind: SessionKind::Service,
    });
    let active = site_approvals::active_capabilities(state, principal, correlation, site)
        .await
        .map_err(|_| {
            tracing::warn!(
                "the active manifest of a site could not be read; the check fails closed"
            );
            FailureKind::UpstreamUnavailable
        })?;
    Ok(Some(
        approved
            .into_iter()
            .filter(|capability| active.contains(capability))
            .collect(),
    ))
}

/// Require `capability` in the effective set of `site` for the site owner session `principal`.
///
/// # Errors
///
/// [`FailureKind::CapabilityNotGranted`] when the set lacks the capability,
/// [`FailureKind::Unauthenticated`] when the site has no approved manifest (its owner sessions end
/// with its approval), and [`FailureKind::UpstreamUnavailable`] when the set cannot be read.
pub(crate) async fn require(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
    capability: &str,
) -> Result<(), FailureKind> {
    match effective_set(state, Some(principal), correlation, site).await? {
        Some(set) if set.iter().any(|granted| granted == capability) => Ok(()),
        Some(_) => Err(FailureKind::CapabilityNotGranted),
        None => Err(FailureKind::Unauthenticated),
    }
}

/// Whether the public `capability` is in the effective set of `site`. `false` for a site that is
/// not approved, has no such id, or did not request the capability: Edge answers all three as an
/// unknown site.
///
/// # Errors
///
/// [`FailureKind::UpstreamUnavailable`] when the set cannot be read.
pub(crate) async fn public(
    state: &ApiState,
    correlation: &str,
    site: &str,
    capability: SiteCapability,
) -> Result<bool, FailureKind> {
    if SiteId::parse(site).is_err() {
        return Ok(false);
    }
    Ok(effective_set(state, None, correlation, site)
        .await?
        .is_some_and(|set| set.iter().any(|granted| granted == capability.as_str())))
}
