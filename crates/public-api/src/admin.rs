//! Shared authorization for deployment-wide operational inspection.

use std::sync::Arc;

use axum::response::Response;
use platform_core::FailureKind;
use ratatoskr_operational_contracts::PLATFORM_OWNER_GRANT;

use crate::{ApiState, Principal};

/// Require the caller's live owner grant for this request.
pub(crate) async fn require_owner(
    state: &Arc<ApiState>,
    principal: Principal,
) -> Result<(), Response> {
    match platform_identity::grant::holds(
        state.database.pool(),
        principal.user_id,
        PLATFORM_OWNER_GRANT,
        jiff::Timestamp::now(),
    )
    .await
    {
        Ok(true) => Ok(()),
        Ok(false) => Err(platform_http::reject(FailureKind::Forbidden)),
        Err(error) => {
            tracing::error!(%error, "owner authorization could not be read");
            Err(platform_http::reject(FailureKind::RequestTimeout))
        }
    }
}
