//! The site event stream `GET /v1/sites/{site}/events` (workspace change
//! `add-site-owner-editing-and-live`, design D5; spec `site-live-events`).
//!
//! One fan-out in Edge: Edge's durable `platform_site_events` feeds [`SiteEventProjection`], which
//! appends each event to a buffer of its site in [`SiteEventHub`]. Every connection reads from that
//! buffer, so the cost of the bus does not grow with the number of open pages. A buffer keeps the
//! last 256 events or the last 24 hours, whichever is smaller. An event id is `<epoch>.<sequence>`:
//! the sequence increases within a site, and the epoch changes with every start of Edge, so an id
//! from before a start is unknown and the stream answers it with `resync`.
//!
//! Anonymous connections have a bound per client and a bound per site; the client is the key of
//! the `public` class (`X-Ratatoskr-Client-IP` from the sites gateway only). The connections of the
//! site's owner sessions count against neither.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use futures_util::stream::Stream;
use platform_core::FailureKind;
use platform_eventing::inbox::Outcome;
use platform_eventing::{EventingError, Handler, Incoming};
use ratatoskr_identifiers::{DigestHex, NotesConflictId, OperationId};
use ratatoskr_sites_contracts::{
    DraftChanged, DraftNotice, EditCompletedNotice, EditConflictedNotice, ReleaseNotice,
    ReleasePublished, SiteEvent, SourceRef,
};
use tokio::sync::broadcast;

/// Edge's durable that feeds the hub, and its two filters (`platform_eventing::stream`).
pub use platform_eventing::{SITE_EVENTS_CONSUMER, SITE_EVENTS_SUBJECTS};

/// The most events a site keeps for a resume.
const WINDOW_EVENTS: usize = 256;

/// The oldest event a site keeps for a resume.
const WINDOW_AGE: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// The most sites whose buffers the hub keeps. Approved sites are few; the bound keeps a flood of
/// site ids from growing the map.
const SITES_MAX: usize = 1024;

/// How often an idle stream sends a comment.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The bounds of the anonymous connections of the stream (design D5; the defaults are starting
/// values to measure).
#[derive(Debug, Clone, Copy)]
pub struct SiteStreamLimits {
    /// Anonymous connections of one client on one site.
    pub per_client: usize,
    /// Anonymous connections on one site.
    pub per_site: usize,
    /// Connections of the owner sessions of one site.
    pub per_site_owner: usize,
    /// How often an owner connection checks that its session is still live.
    pub owner_check: Duration,
}

impl Default for SiteStreamLimits {
    fn default() -> Self {
        Self {
            per_client: 4,
            per_site: 64,
            per_site_owner: 16,
            owner_check: KEEP_ALIVE * 4,
        }
    }
}

/// One stored event.
#[derive(Debug, Clone)]
struct Stored {
    sequence: u64,
    at: jiff::Timestamp,
    event: SiteEvent,
}

/// The buffer and the anonymous connections of one site.
#[derive(Debug, Default)]
struct SiteLog {
    /// The sequence of the last event, 0 before the first.
    last: u64,
    events: VecDeque<Stored>,
    anonymous: usize,
    clients: HashMap<String, usize>,
}

/// The per-site buffers of the site event stream.
#[derive(Debug)]
pub struct SiteEventHub {
    epoch: String,
    sites: Mutex<HashMap<String, SiteLog>>,
    wake: broadcast::Sender<String>,
}

impl Default for SiteEventHub {
    fn default() -> Self {
        let (wake, _) = broadcast::channel(1024);
        Self {
            epoch: uuid::Uuid::now_v7().simple().to_string(),
            sites: Mutex::new(HashMap::new()),
            wake,
        }
    }
}

/// What a connection reads next.
enum Read {
    /// The events after the cursor that the caller may see, and the new cursor.
    Events(Vec<(u64, SiteEvent)>, u64),
    /// Some events after the cursor are no longer kept: `resync`, then after this sequence.
    Resync(u64),
}

impl SiteEventHub {
    /// Append `event` to the buffer of `site`, at `now`, and wake its connections.
    pub fn push(&self, site: &str, event: SiteEvent, now: jiff::Timestamp) {
        {
            let mut sites = self.sites.lock().unwrap_or_else(PoisonError::into_inner);
            if !sites.contains_key(site) && sites.len() >= SITES_MAX {
                tracing::warn!("the site event hub is full; an event of a new site is dropped");
                return;
            }
            let log = sites.entry(site.to_owned()).or_default();
            log.last += 1;
            let sequence = log.last;
            log.events.push_back(Stored {
                sequence,
                at: now,
                event,
            });
            while log.events.len() > WINDOW_EVENTS
                || log
                    .events
                    .front()
                    .is_some_and(|oldest| oldest.at < now - WINDOW_AGE)
            {
                log.events.pop_front();
            }
        }
        let _ = self.wake.send(site.to_owned());
    }

    fn id(&self, sequence: u64) -> String {
        format!("{}.{sequence}", self.epoch)
    }

    /// The cursor of a connection that sends `last_event_id`: the sequence of that event when this
    /// process issued it and it is not newer than the last event; else `None`, and the connection
    /// starts with `resync`.
    fn cursor_of(&self, site: &str, last_event_id: &str) -> Option<u64> {
        let last = self.read(site, u64::MAX, true);
        let last = match last {
            Read::Events(_, last) | Read::Resync(last) => last,
        };
        last_event_id
            .split_once('.')
            .filter(|(epoch, _)| *epoch == self.epoch)
            .and_then(|(_, sequence)| sequence.parse::<u64>().ok())
            .filter(|sequence| *sequence <= last)
    }

    /// The events of `site` after `cursor` that the caller may see. Under one lock, so that the new
    /// cursor and the events agree.
    fn read(&self, site: &str, cursor: u64, owner: bool) -> Read {
        let sites = self.sites.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(log) = sites.get(site) else {
            return Read::Events(Vec::new(), 0);
        };
        if cursor < log.last
            && log
                .events
                .front()
                .is_some_and(|oldest| cursor + 1 < oldest.sequence)
        {
            return Read::Resync(log.last);
        }
        let events = log
            .events
            .iter()
            .filter(|stored| stored.sequence > cursor)
            .filter(|stored| owner || matches!(stored.event, SiteEvent::ReleasePublished(_)))
            .map(|stored| (stored.sequence, stored.event.clone()))
            .collect();
        Read::Events(events, log.last)
    }

    /// Count one anonymous connection of `client` on `site`, or refuse it above a bound.
    fn admit(
        self: &Arc<Self>,
        site: &str,
        client: &str,
        limits: SiteStreamLimits,
    ) -> Option<Admitted> {
        let mut sites = self.sites.lock().unwrap_or_else(PoisonError::into_inner);
        if !sites.contains_key(site) && sites.len() >= SITES_MAX {
            return None;
        }
        let log = sites.entry(site.to_owned()).or_default();
        let of_client = log.clients.get(client).copied().unwrap_or(0);
        if log.anonymous >= limits.per_site || of_client >= limits.per_client {
            return None;
        }
        log.anonymous += 1;
        log.clients.insert(client.to_owned(), of_client + 1);
        Some(Admitted {
            hub: Arc::clone(self),
            site: site.to_owned(),
            client: client.to_owned(),
        })
    }
}

/// One counted anonymous connection; dropping it, when the client goes away, releases the count.
struct Admitted {
    hub: Arc<SiteEventHub>,
    site: String,
    client: String,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        let mut sites = self
            .hub
            .sites
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(log) = sites.get_mut(&self.site) {
            log.anonymous = log.anonymous.saturating_sub(1);
            if let Some(count) = log.clients.get_mut(&self.client) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    log.clients.remove(&self.client);
                }
            }
        }
    }
}

/// One connection of the stream, after the capability check: `owner` for a connection of a site
/// owner session of the site, else `client` is the client key of the anonymous connection.
pub(crate) struct Connection<'a> {
    pub(crate) site: &'a str,
    pub(crate) owner: bool,
    pub(crate) client: &'a str,
    pub(crate) last_event_id: Option<&'a str>,
}

/// `GET /v1/sites/{site}/events`: the stream of one connection, or the retryable overloaded error
/// above a bound of anonymous connections.
pub(crate) fn stream(
    hub: &Arc<SiteEventHub>,
    limits: SiteStreamLimits,
    connection: &Connection<'_>,
) -> Response {
    let admitted = if connection.owner {
        None
    } else {
        match hub.admit(connection.site, connection.client, limits) {
            Some(admitted) => Some(admitted),
            None => return platform_http::reject(FailureKind::Overloaded),
        }
    };
    let mut reader = Reader {
        hub: Arc::clone(hub),
        site: connection.site.to_owned(),
        owner: connection.owner,
        wake: hub.wake.subscribe(),
        pending: VecDeque::new(),
        cursor: 0,
        _admitted: admitted,
    };
    let resumed = connection
        .last_event_id
        .map(|raw| hub.cursor_of(connection.site, raw));
    match resumed {
        None => reader.cursor = reader.last(),
        Some(Some(cursor)) => reader.cursor = cursor,
        Some(None) => {
            let last = reader.last();
            reader.resync(last);
        }
    }
    Sse::new(reader.events())
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// The state of one connection.
struct Reader {
    hub: Arc<SiteEventHub>,
    site: String,
    owner: bool,
    wake: broadcast::Receiver<String>,
    pending: VecDeque<Event>,
    cursor: u64,
    _admitted: Option<Admitted>,
}

impl Reader {
    fn events(self) -> impl Stream<Item = Result<Event, Infallible>> + Send {
        futures_util::stream::unfold(self, |mut reader| async move {
            loop {
                if let Some(event) = reader.pending.pop_front() {
                    return Some((Ok(event), reader));
                }
                reader.fill();
                if !reader.pending.is_empty() {
                    continue;
                }
                match reader.wake.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
    }

    fn last(&self) -> u64 {
        match self.hub.read(&self.site, u64::MAX, true) {
            Read::Events(_, last) | Read::Resync(last) => last,
        }
    }

    /// Queue `resync` and continue after `last`.
    fn resync(&mut self, last: u64) {
        self.cursor = last;
        self.pending
            .push_back(frame(&self.hub.id(last), &SiteEvent::Resync));
    }

    /// Queue the events after the cursor, or `resync` when some are no longer kept.
    fn fill(&mut self) {
        match self.hub.read(&self.site, self.cursor, self.owner) {
            Read::Events(events, last) => {
                for (sequence, event) in events {
                    self.pending
                        .push_back(frame(&self.hub.id(sequence), &event));
                }
                self.cursor = self.cursor.max(last);
            }
            Read::Resync(last) => self.resync(last),
        }
    }
}

/// One SSE block: the id, the event type and the `sites/site-event.v1` document as data.
fn frame(id: &str, event: &SiteEvent) -> Event {
    let document = serde_json::to_value(event).unwrap_or_default();
    let kind = document
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("resync")
        .to_owned();
    Event::default()
        .id(id)
        .event(kind)
        .data(document.to_string())
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
        message: &Incoming,
        now: jiff::Timestamp,
    ) -> Result<Outcome, EventingError> {
        let Some(payload) = message.payload.get("payload").cloned() else {
            return Ok(Outcome::Rejected);
        };
        let (site, event) = match message.subject.as_str() {
            "evt.sites.release.published.v1" => {
                let Ok(release) = serde_json::from_value::<ReleasePublished>(payload) else {
                    return Ok(Outcome::Rejected);
                };
                (
                    release.site_id,
                    SiteEvent::ReleasePublished(ReleaseNotice {
                        release_id: release.release_id,
                        published_at: release.published_at,
                    }),
                )
            }
            "evt.sites.draft.changed.v1" => {
                let Ok(draft) = serde_json::from_value::<DraftChanged>(payload) else {
                    return Ok(Outcome::Rejected);
                };
                (
                    draft.site_id,
                    SiteEvent::DraftChanged(DraftNotice {
                        source_ref: draft.source_ref,
                        revision: draft.revision,
                        slug: draft.slug,
                    }),
                )
            }
            _ => return Ok(Outcome::Rejected),
        };
        self.hub.push(site.as_str(), event, now);
        Ok(Outcome::Applied)
    }
}

/// Applies the reports of every producer to the operation projection, as Edge always did, and
/// sends `edit.completed` to the owner sessions of the site of an edit when its operation succeeds:
/// the vault has the write, and the edit's content is the note's revision.
#[derive(Debug, Clone)]
pub struct SiteEditReports {
    /// The hub that the streams read.
    pub hub: Arc<SiteEventHub>,
}

impl Handler for SiteEditReports {
    async fn handle(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
        now: jiff::Timestamp,
    ) -> Result<Outcome, EventingError> {
        let outcome = platform_operations::ProgressProjection
            .handle(transaction, message, now)
            .await?;
        if outcome != Outcome::Applied {
            return Ok(outcome);
        }
        let Some(operation_id) = operation_of(message) else {
            return Ok(outcome);
        };
        let completed: Option<(String, String, String)> = sqlx::query_as(
            "select e.site_id, e.source_ref, e.revision
               from operations.site_edits e
               join operations.operations o on o.operation_id = e.operation_id
              where e.operation_id = $1 and o.status = 'succeeded'",
        )
        .bind(operation_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(query_failed)?;
        if let Some((site, source_ref, revision)) = completed
            && let Some(event) = edit_completed(operation_id, &source_ref, &revision)
        {
            self.hub.push(&site, event, now);
        }
        Ok(outcome)
    }
}

/// Applies the conflict events of `ratatoskr-notes` to the operation projection, as Edge always
/// did, and sends `edit.conflicted` to the owner sessions of the site of an edit that met a
/// conflict.
#[derive(Debug, Clone)]
pub struct SiteEditConflicts {
    /// The hub that the streams read.
    pub hub: Arc<SiteEventHub>,
}

impl Handler for SiteEditConflicts {
    async fn handle(
        &self,
        transaction: &mut sqlx::PgTransaction<'_>,
        message: &Incoming,
        now: jiff::Timestamp,
    ) -> Result<Outcome, EventingError> {
        let outcome = platform_operations::NotesConflictProjection
            .handle(transaction, message, now)
            .await?;
        if outcome != Outcome::Applied || message.subject.as_str() != "evt.notes.conflict.raised.v1"
        {
            return Ok(outcome);
        }
        let payload = message.payload.get("payload");
        let conflict = payload
            .and_then(|payload| payload.get("conflict_id"))
            .cloned()
            .and_then(|id| serde_json::from_value::<NotesConflictId>(id).ok());
        let (Some(operation_id), Some(conflict_id)) = (operation_of(message), conflict) else {
            return Ok(outcome);
        };
        let edit: Option<(String, String)> = sqlx::query_as(
            "select site_id, source_ref from operations.site_edits where operation_id = $1",
        )
        .bind(operation_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(query_failed)?;
        if let Some((site, source_ref)) = edit
            && let Ok(source_ref) = SourceRef::parse(&source_ref)
        {
            let event = SiteEvent::EditConflicted(EditConflictedNotice {
                operation_id: OperationId(operation_id),
                conflict_id,
                source_ref,
            });
            self.hub.push(&site, event, now);
        }
        Ok(outcome)
    }
}

/// The operation that a report or a conflict event names.
fn operation_of(message: &Incoming) -> Option<uuid::Uuid> {
    message
        .payload
        .get("payload")?
        .get("operation_id")?
        .as_str()?
        .parse()
        .ok()
}

fn edit_completed(operation_id: uuid::Uuid, source_ref: &str, revision: &str) -> Option<SiteEvent> {
    Some(SiteEvent::EditCompleted(EditCompletedNotice {
        operation_id: OperationId(operation_id),
        source_ref: SourceRef::parse(source_ref).ok()?,
        revision: DigestHex::parse(revision).ok()?,
    }))
}

fn query_failed(error: sqlx::Error) -> EventingError {
    EventingError::Persistence(platform_persistence::PersistenceError::Query(error))
}
