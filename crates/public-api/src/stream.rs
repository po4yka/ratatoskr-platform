//! Streaming an operation's progress.
//!
//! `ARCHITECTURE.md` S5.5: "SSE connections read from persisted operation state and transient
//! notifications. The event bus is not exposed directly to clients." So this reads
//! `operations.operation_progress`, never `NATS`. A client's connection cannot therefore observe a
//! message that was not durably recorded, and a client cannot be used to reach the bus.
//!
//! S14: "SSE disconnects do not affect operation execution." Nothing here writes, and dropping the
//! stream drops only the reader.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use futures_util::stream::Stream;
use http::HeaderMap;
use platform_core::FailureKind;
use platform_operations::transition;
use uuid::Uuid;

use crate::{ApiState, Principal};

/// How often the stream looks for new entries.
///
/// ponytail: a poll, not `LISTEN`/`NOTIFY`. One statement per connection per second is affordable at
/// this scale and has no failure mode of its own; a notification channel is the upgrade when
/// connection counts make it measurable, and it changes nothing a client can see.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The most entries one poll will read. A bound, so a long-running operation with a chatty producer
/// cannot make one connection read unboundedly.
const PAGE: i64 = 256;

/// `GET /v2/operations/{operation_id}/events`.
///
/// Replays from `Last-Event-ID` when the client reconnects, then follows. The stream ends when the
/// operation reaches a terminal status, because there will never be another entry: leaving it open
/// would hold a connection for nothing.
pub async fn events(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(operation_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let pool = state.database.pool();

    // The same ownership rule and the same refusal as the polling route: another principal's
    // operation and a nonexistent one are indistinguishable.
    match platform_operations::find(pool, operation_id).await {
        Ok(Some(operation)) if operation.owner_user_id == principal.user_id => {}
        Ok(_) => return platform_http::reject(FailureKind::NotFound),
        Err(error) => {
            tracing::error!(%error, "the operation could not be read");
            return platform_http::reject(FailureKind::RequestTimeout);
        }
    }

    let cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.trim().parse::<Uuid>().ok());

    Sse::new(progress_stream(state, operation_id, cursor))
        // A comment line every fifteen seconds. Without it a proxy closes an idle connection and the
        // client sees a disconnect it has to distinguish from a finished operation.
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// The entries after `cursor`, then whatever arrives, then the end.
fn progress_stream(
    state: Arc<ApiState>,
    operation_id: Uuid,
    cursor: Option<Uuid>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    async_stream::stream! {
        let mut cursor = cursor;
        loop {
            let entries = platform_operations::progress_since(
                state.database.pool(),
                operation_id,
                cursor,
                PAGE,
            )
            .await;

            let entries = match entries {
                Ok(entries) => entries,
                Err(error) => {
                    // The stream ends rather than reporting a failure inside it: an SSE body has
                    // already committed to 200, so an error event would be a second, contradictory
                    // status. The client reconnects with `Last-Event-ID` and loses nothing.
                    tracing::error!(%error, "the progress stream could not be read");
                    break;
                }
            };

            let mut terminal = false;
            for entry in entries {
                cursor = Some(entry.progress_id);
                terminal = transition::is_terminal(entry.status);
                let data = serde_json::to_string(&entry).unwrap_or_default();
                yield Ok(Event::default()
                    .id(entry.progress_id.to_string())
                    .event("progress")
                    .data(data));
            }

            if terminal {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
