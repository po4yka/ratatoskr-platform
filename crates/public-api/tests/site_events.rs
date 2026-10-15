//! The site event stream `GET /v1/sites/{site}/events` (workspace change
//! `add-site-owner-editing-and-live`, spec `site-live-events`). Events reach the stream through the
//! projection that Edge's durable `platform_site_events` feeds, driven here through
//! `consumer::deliver`, which is what the consumer calls for each message.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in a test binary"
)]

mod site_owner_support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use http::Request;
use http_body_util::BodyExt as _;
use platform_eventing::{Incoming, Subject};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::ApiState;
use platform_public_api::site_events::{SiteEventHub, SiteEventProjection, SiteStreamLimits};
use serde_json::{Value, json};
use site_owner_support::{COOKIE, HOSTNAME, OWNER, SITE, Stub, approval, primary, sign_in};
use tower::ServiceExt as _;
use uuid::Uuid;

const CAPABILITIES: &[&str] = &["pages.read", "live.release", "draft.read", "notes.edit"];
const GATEWAY: &str = "127.0.0.83:40000";

/// An Edge with `site-a` approved and active, a site owner cookie, and the hub that its streams
/// read. `limits` sets the bounds of anonymous streams.
async fn edge(
    harness: &TestDatabase,
    limits: SiteStreamLimits,
) -> (Router, String, Arc<SiteEventHub>) {
    let stub = Stub::start().await;
    stub.manifest(SITE, HOSTNAME, CAPABILITIES);
    let owner = primary(harness.pool(), OWNER, true).await;
    approval(
        harness.pool(),
        owner,
        SITE,
        HOSTNAME,
        CAPABILITIES,
        "approved",
    )
    .await;
    let (app, hub) = with_hub(harness, stub.address, limits);
    let cookie = sign_in(&app, SITE, HOSTNAME).await;
    // The stub must outlive the test; its task is detached, so it does.
    (app, cookie, hub)
}

/// An Edge over a new hub, as after a start of the process.
fn with_hub(
    harness: &TestDatabase,
    sites: SocketAddr,
    limits: SiteStreamLimits,
) -> (Router, Arc<SiteEventHub>) {
    let mut state: ApiState = site_owner_support::state(harness, sites, &[]);
    state.site_stream_limits = limits;
    let hub = Arc::clone(&state.site_events);
    (site_owner_support::app(state), hub)
}

/// Open the stream of `site-a`: as the owner with `cookie`, or else anonymously from the gateway
/// for the client `client`.
async fn open(
    app: &Router,
    cookie: Option<&str>,
    client: &str,
    last_event_id: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .uri("/v1/sites/site-a/events")
        .header("x-ratatoskr-client-ip", client);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", format!("{COOKIE}={cookie}"));
    }
    if let Some(id) = last_event_id {
        builder = builder.header("last-event-id", id);
    }
    let mut request = builder.body(Body::empty()).expect("a request");
    let peer: SocketAddr = GATEWAY.parse().expect("an address");
    request.extensions_mut().insert(ConnectInfo(peer));
    app.clone().oneshot(request).await.expect("a response")
}

/// One SSE block of the stream: its id, its type and its data; a keep-alive comment is `None`.
#[derive(Debug, Clone, PartialEq)]
struct Received {
    id: Option<String>,
    data: Value,
}

/// The next block of `body`, or `None` after `wait`.
async fn next_block(body: &mut Body, buffer: &mut String, wait: Duration) -> Option<String> {
    loop {
        if let Some((block, rest)) = buffer.split_once("\n\n") {
            let (block, rest) = (block.to_owned(), rest.to_owned());
            *buffer = rest;
            return Some(block);
        }
        let frame = tokio::time::timeout(wait, body.frame()).await.ok()??.ok()?;
        if let Ok(data) = frame.into_data() {
            buffer.push_str(&String::from_utf8_lossy(&data));
        }
    }
}

/// The next event of `body` that is not a keep-alive comment.
async fn next_event(body: &mut Body, buffer: &mut String) -> Option<Received> {
    loop {
        let block = next_block(body, buffer, Duration::from_secs(5)).await?;
        let mut id = None;
        let mut data = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("id:") {
                id = Some(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("data:") {
                data = serde_json::from_str(value.trim()).ok();
            }
        }
        if let Some(data) = data {
            return Some(Received { id, data });
        }
    }
}

/// A sites event of `subject` for `site-a`, as `ratatoskr-sites` publishes it.
fn message(subject: &str, payload: &Value) -> Incoming {
    Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::parse(subject).expect("a subject"),
        producer: "ratatoskr-sites".to_owned(),
        payload: json!({
            "event_id": Uuid::now_v7(),
            "producer": "ratatoskr-sites",
            "tenant_id": "site:site-a",
            "payload": payload.clone(),
        }),
    }
}

fn released(release: char) -> Incoming {
    message(
        "evt.sites.release.published.v1",
        &json!({
            "site_id": "site-a",
            "release_id": release.to_string().repeat(64),
            "publication_ids": ["garden"],
            "page_count": 4,
            "published_at": "2026-10-15T08:00:00Z",
        }),
    )
}

fn draft(revision: char) -> Incoming {
    message(
        "evt.sites.draft.changed.v1",
        &json!({
            "site_id": "site-a",
            "source_ref": "src_garden_start",
            "revision": revision.to_string().repeat(64),
            "slug": "start",
        }),
    )
}

async fn deliver(pool: &sqlx::PgPool, hub: &Arc<SiteEventHub>, incoming: &Incoming) {
    let projection = SiteEventProjection {
        hub: Arc::clone(hub),
    };
    platform_eventing::consumer::deliver(pool, &projection, incoming, jiff::Timestamp::now())
        .await
        .expect("the projection applies the event");
}

/// Spec `site-live-events`, "An anonymous page and the owner watch the same site".
#[tokio::test]
async fn anonymous_sees_only_release_published() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, cookie, hub) = edge(&harness, SiteStreamLimits::default()).await;
    let owner = open(&app, Some(&cookie), "5.5.5.5", None).await;
    let anonymous = open(&app, None, "6.6.6.6", None).await;
    assert_eq!(owner.status(), 200, "the owner stream");
    assert_eq!(anonymous.status(), 200, "the anonymous stream");
    let (mut owner, mut anonymous) = (owner.into_body(), anonymous.into_body());

    deliver(harness.pool(), &hub, &draft('d')).await;
    deliver(harness.pool(), &hub, &released('r')).await;

    let (mut seen, mut buffer) = (Vec::new(), String::new());
    for _ in 0..2 {
        seen.push(
            next_event(&mut owner, &mut buffer)
                .await
                .expect("an owner event")
                .data["type"]
                .clone(),
        );
    }
    assert_eq!(seen, ["draft.changed", "release.published"]);
    let mut buffer = String::new();
    let first = next_event(&mut anonymous, &mut buffer)
        .await
        .expect("an anonymous event");
    assert_eq!(first.data["type"], "release.published", "{first:?}");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-live-events`, "The release event shape".
#[tokio::test]
async fn release_payload_holds_only_id_and_time() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, _, hub) = edge(&harness, SiteStreamLimits::default()).await;
    let response = open(&app, None, "6.6.6.6", None).await;
    assert_eq!(response.status(), 200, "the anonymous stream");
    let mut body = response.into_body();

    deliver(harness.pool(), &hub, &released('r')).await;

    let event = next_event(&mut body, &mut String::new())
        .await
        .expect("an event");
    assert_eq!(
        event.data,
        json!({
            "type": "release.published",
            "payload": {"release_id": "r".repeat(64), "published_at": "2026-10-15T08:00:00Z"},
        })
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-live-events`, "A short break loses nothing".
#[tokio::test]
async fn resume_inside_window_replays_in_order() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, cookie, hub) = edge(&harness, SiteStreamLimits::default()).await;
    let response = open(&app, Some(&cookie), "5.5.5.5", None).await;
    assert_eq!(response.status(), 200, "the owner stream");
    let mut body = response.into_body();
    deliver(harness.pool(), &hub, &draft('a')).await;
    let first = next_event(&mut body, &mut String::new())
        .await
        .expect("an event");
    let last_id = first.id.expect("an event id");
    drop(body);

    for revision in ['b', 'c', 'd'] {
        deliver(harness.pool(), &hub, &draft(revision)).await;
    }
    let response = open(&app, Some(&cookie), "5.5.5.5", Some(&last_id)).await;
    assert_eq!(response.status(), 200, "the resumed stream");
    let mut body = response.into_body();
    let mut buffer = String::new();
    let mut revisions = Vec::new();
    for _ in 0..3 {
        let event = next_event(&mut body, &mut buffer)
            .await
            .expect("a replayed event");
        assert_eq!(event.data["type"], "draft.changed", "{event:?}");
        revisions.push(
            event.data["payload"]["revision"]
                .as_str()
                .unwrap()
                .chars()
                .next()
                .unwrap(),
        );
    }
    assert_eq!(
        revisions,
        ['b', 'c', 'd'],
        "the three events in order, without resync"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-live-events`, "A long break asks for a refetch": the window holds 256 events.
#[tokio::test]
async fn resume_outside_window_starts_with_resync() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, cookie, hub) = edge(&harness, SiteStreamLimits::default()).await;
    let response = open(&app, Some(&cookie), "5.5.5.5", None).await;
    assert_eq!(response.status(), 200, "the owner stream");
    let mut body = response.into_body();
    deliver(harness.pool(), &hub, &draft('a')).await;
    let first = next_event(&mut body, &mut String::new())
        .await
        .expect("an event");
    drop(body);
    for _ in 0..300 {
        deliver(harness.pool(), &hub, &draft('b')).await;
    }

    let response = open(&app, Some(&cookie), "5.5.5.5", first.id.as_deref()).await;
    assert_eq!(response.status(), 200, "the resumed stream");
    let event = next_event(&mut response.into_body(), &mut String::new())
        .await
        .expect("an event");
    assert_eq!(event.data, json!({"type": "resync"}), "{event:?}");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-live-events`, "Edge restarted": an id from before the start is unknown.
#[tokio::test]
async fn restart_starts_with_resync() {
    let harness = TestDatabase::create().await.expect("a test database");
    let stub = Stub::start().await;
    stub.manifest(SITE, HOSTNAME, CAPABILITIES);
    let owner = primary(harness.pool(), OWNER, true).await;
    approval(
        harness.pool(),
        owner,
        SITE,
        HOSTNAME,
        CAPABILITIES,
        "approved",
    )
    .await;
    let (app, hub) = with_hub(&harness, stub.address, SiteStreamLimits::default());
    let response = open(&app, None, "6.6.6.6", None).await;
    assert_eq!(response.status(), 200, "the anonymous stream");
    let mut body = response.into_body();
    deliver(harness.pool(), &hub, &released('a')).await;
    let first = next_event(&mut body, &mut String::new())
        .await
        .expect("an event");
    drop(body);

    let (restarted, _) = with_hub(&harness, stub.address, SiteStreamLimits::default());
    let response = open(&restarted, None, "6.6.6.6", first.id.as_deref()).await;
    assert_eq!(response.status(), 200, "the stream after the start");
    let event = next_event(&mut response.into_body(), &mut String::new())
        .await
        .expect("an event");
    assert_eq!(event.data, json!({"type": "resync"}), "{event:?}");

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-live-events`, "An idle stream": two keep-alive comments in 40 seconds.
#[tokio::test]
async fn keep_alive_every_fifteen_seconds() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, _, _) = edge(&harness, SiteStreamLimits::default()).await;
    let response = open(&app, None, "6.6.6.6", None).await;
    assert_eq!(response.status(), 200, "the anonymous stream");
    let mut body = response.into_body();

    tokio::time::pause();
    let mut buffer = String::new();
    let mut comments = 0;
    for _ in 0..8 {
        tokio::time::advance(Duration::from_secs(5)).await;
        while let Some(block) = next_block(&mut body, &mut buffer, Duration::from_millis(10)).await
        {
            if block.lines().all(|line| line.starts_with(':')) {
                comments += 1;
            }
        }
    }
    tokio::time::resume();
    assert!(
        comments >= 2,
        "{comments} keep-alive comments in 40 seconds"
    );

    harness.cleanup().await.expect("cleanup");
}

/// The status and the retry class of a refused stream.
async fn refusal(response: axum::response::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Spec `site-live-events`, "One client opens too many streams".
#[tokio::test]
async fn per_client_bound_refuses_fifth_anonymous_stream() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (app, _, _) = edge(&harness, SiteStreamLimits::default()).await;
    let mut open_streams = Vec::new();
    for index in 0..4 {
        let response = open(&app, None, "7.7.7.7", None).await;
        assert_eq!(response.status(), 200, "stream {index} of the client");
        open_streams.push(response);
    }

    let (status, body) = refusal(open(&app, None, "7.7.7.7", None).await).await;
    assert_eq!(
        (status, body["code"].as_str(), body["retryable"].as_bool()),
        (503, Some("platform.limit.overloaded"), Some(true)),
        "the fifth stream of one client: {body}"
    );
    let other = open(&app, None, "8.8.8.8", None).await;
    assert_eq!(other.status(), 200, "another client opens its first stream");
    drop(open_streams);

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-live-events`, "The per-site bound is reached".
#[tokio::test]
async fn anonymous_bound_spares_owner() {
    let harness = TestDatabase::create().await.expect("a test database");
    let limits = SiteStreamLimits {
        per_client: 4,
        per_site: 2,
    };
    let (app, cookie, _) = edge(&harness, limits).await;
    let first = open(&app, None, "1.1.1.1", None).await;
    let second = open(&app, None, "2.2.2.2", None).await;
    assert_eq!(
        (first.status().as_u16(), second.status().as_u16()),
        (200, 200),
        "two anonymous streams"
    );

    let (status, body) = refusal(open(&app, None, "3.3.3.3", None).await).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (503, Some("platform.limit.overloaded")),
        "the third anonymous stream: {body}"
    );
    let owner = open(&app, Some(&cookie), "4.4.4.4", None).await;
    assert_eq!(
        owner.status(),
        200,
        "the owner stream counts against neither bound"
    );
    drop((first, second, owner));

    harness.cleanup().await.expect("cleanup");
}
