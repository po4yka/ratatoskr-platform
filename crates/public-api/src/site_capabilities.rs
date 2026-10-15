//! The effective capability set of a site at Edge (workspace change
//! `add-site-owner-editing-and-live`, design D3; specs `site-capability-enforcement` and
//! `site-manifest`).
//!
//! The effective set is the capabilities of the last approved manifest
//! (`identity.site_approvals`) that the active manifest still declares (the sites service). Edge
//! checks a capability against it before it forwards a request or reads data for it. When Edge
//! cannot read the set, the check fails closed with the retryable unavailable error.

use platform_core::FailureKind;

use crate::{ApiState, Principal, site_approvals};

/// `notes.edit`: edit the source notes of the site's publications, and request a release.
pub(crate) const NOTES_EDIT: &str = "notes.edit";

/// The effective capability set of `site`, or `None` when the site has no approved manifest.
///
/// # Errors
///
/// [`FailureKind::UpstreamUnavailable`] when the approval or the active manifest cannot be read.
pub(crate) async fn effective_set(
    state: &ApiState,
    principal: Principal,
    correlation: &str,
    site: &str,
) -> Result<Option<Vec<String>>, FailureKind> {
    let approved: Option<Vec<String>> = sqlx::query_scalar(
        "select capabilities from identity.site_approvals
          where site_id = $1 and state = 'approved'",
    )
    .bind(site)
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| {
        tracing::error!(%error, "the approval of a site could not be read");
        FailureKind::UpstreamUnavailable
    })?;
    let Some(approved) = approved else {
        return Ok(None);
    };
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
    match effective_set(state, principal, correlation, site).await? {
        Some(set) if set.iter().any(|granted| granted == capability) => Ok(()),
        Some(_) => Err(FailureKind::CapabilityNotGranted),
        None => Err(FailureKind::Unauthenticated),
    }
}
