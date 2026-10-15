//! The site event stream `GET /v1/sites/{site}/events` (workspace change
//! `add-site-owner-editing-and-live`, design D5; spec `site-live-events`).

use std::sync::Arc;

use platform_eventing::inbox::Outcome;
use platform_eventing::{EventingError, Handler, Incoming};

/// The per-site buffers of the site event stream.
#[derive(Debug, Default)]
pub struct SiteEventHub;

/// The bounds of the anonymous connections of the stream.
#[derive(Debug, Clone, Copy)]
pub struct SiteStreamLimits {
    /// Anonymous connections of one client on one site.
    pub per_client: usize,
    /// Anonymous connections on one site.
    pub per_site: usize,
}

impl Default for SiteStreamLimits {
    fn default() -> Self {
        Self {
            per_client: 4,
            per_site: 64,
        }
    }
}

/// Applies `evt.sites.release.published.v1` and `evt.sites.draft.changed.v1` to the hub.
#[derive(Debug, Clone)]
pub struct SiteEventProjection {
    /// The hub that the streams read.
    pub hub: Arc<SiteEventHub>,
}

impl Handler for SiteEventProjection {
    async fn handle(
        &self,
        _transaction: &mut sqlx::PgTransaction<'_>,
        _message: &Incoming,
        _now: jiff::Timestamp,
    ) -> Result<Outcome, EventingError> {
        // A stub for workspace task 7.1.
        Ok(Outcome::Applied)
    }
}
