//! Owner editing from a site (workspace change `add-site-owner-editing-and-live`, spec
//! `site-owner-editing`): the draft read, the content upload, the edit, its completion and its
//! conflict, and the release that follows, against stub sites and notes services.
//!
//! The loopback questions that Edge asks the sites service are
//! `GET /internal/sites/{site}/sources/{source_ref}` (the connection and the path of a note in the
//! site's publications, 404 for any other) and `GET /internal/sites/{site}/scope?connection_id=&path=`
//! (the reverse: the `source_ref` of a file in scope).

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in a test binary"
)]

mod site_owner_support;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt as _;
use platform_eventing::{Incoming, Subject};
use platform_persistence::test_support::TestDatabase;
use platform_public_api::site_events::{SiteEditConflicts, SiteEditReports, SiteEventHub};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use site_owner_support::{
    COOKIE, HOSTNAME, OWNER, SITE, Stub, approval, from_site, outbox, primary, send, sign_in,
};
use tower::ServiceExt as _;
use uuid::Uuid;

const CAPABILITIES: &[&str] = &["pages.read", "live.release", "draft.read", "notes.edit"];
const CONNECTION: &str = "018f0000-0000-7000-8000-000000000b01";
const VAULT_PATH: &str = "Garden/Start.md";
const SOURCE: &str = "src_garden_start";
const TOKEN: &str = "rst_0v8k4a2j9pm1d7n5tp3es6uabfhij1cm4nop5qrs";
const CONFLICT: &str = "018f0000-0000-7000-8000-000000000e01";
const MARKER: &str = "marker-site-edit-5c1f";
const WRITE: &str = "cmd.notes.file.write_requested.v1";
const BUILD: &str = "cmd.sites.release.build_requested.v1";

fn content() -> String {
    format!("# Start\nnew text {MARKER}\n")
}

fn content_hash() -> String {
    format!("{:x}", Sha256::digest(content().as_bytes()))
}

fn blob_ref() -> Value {
    json!({
        "owner_service": "ratatoskr-notes",
        "digest": {"algorithm": "sha256", "hex": content_hash()},
        "media_type": "text/markdown",
        "length_bytes": content().len(),
    })
}

fn not_found(code: &str) -> (u16, Value) {
    (
        404,
        json!({"code": code, "message": "No such note.", "retryable": false}),
    )
}

struct Edge {
    app: Router,
    cookie: String,
    hub: Arc<SiteEventHub>,
    sites: Stub,
    notes: Stub,
    owner: Uuid,
}

/// An Edge with `site-a` approved and active, stub sites and notes services that answer the
/// fixtures of this file, and a site owner session.
async fn edge(harness: &TestDatabase) -> Edge {
    let sites = Stub::start().await;
    let notes = Stub::start().await;
    sites.manifest(SITE, HOSTNAME, CAPABILITIES);
    {
        let mut answers = sites.answers.lock().unwrap();
        answers.insert(
            format!("/internal/sites/site-a/sources/{SOURCE}"),
            (
                200,
                json!({"connection_id": CONNECTION, "path": VAULT_PATH}),
            ),
        );
        answers.insert(
            "/internal/sites/site-a/sources/src_other".to_owned(),
            not_found("sites.source_not_found"),
        );
        answers.insert(
            "/internal/sites/site-a/scope".to_owned(),
            (200, json!({"source_ref": SOURCE})),
        );
        answers.insert(
            "/v1/sites/site-a/drafts/pages/start".to_owned(),
            (200, draft("old text")),
        );
        answers.insert(
            "/v1/sites/site-a/drafts/sources/src_other".to_owned(),
            not_found("sites.source_not_found"),
        );
        answers.insert(
            "/v1/sites/site-a/content/pages/start".to_owned(),
            (
                200,
                json!({"slug": "start", "title": "Start", "text": "old text"}),
            ),
        );
    }
    {
        let mut answers = notes.answers.lock().unwrap();
        let blobs = format!("/v1/notes/connections/{CONNECTION}/blobs");
        answers.insert(
            blobs.clone(),
            (
                201,
                json!({"resumption_token": TOKEN, "chunk_size_bytes": 16_777_216,
                       "expires_at": "2030-01-01T00:00:00Z"}),
            ),
        );
        answers.insert(
            format!("{blobs}/{TOKEN}/chunks/0"),
            (
                200,
                json!({"resumption_token": TOKEN, "chunk_index": 0, "received_chunks_count": 1,
                       "idempotent_replay": false}),
            ),
        );
        answers.insert(
            format!("{blobs}/{TOKEN}/finalize"),
            (200, json!({"outcome": "stored", "blob_ref": blob_ref()})),
        );
        answers.insert(
            format!("/v1/notes/conflicts/{CONFLICT}"),
            (200, notes_conflict()),
        );
    }
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
    let state = site_owner_support::state(
        harness,
        sites.address,
        &[("notes", "/v1/notes", notes.address)],
    );
    let hub = Arc::clone(&state.site_events);
    let app = site_owner_support::app(state);
    let cookie = sign_in(&app, SITE, HOSTNAME).await;
    Edge {
        app,
        cookie,
        hub,
        sites,
        notes,
        owner,
    }
}

fn draft(text: &str) -> Value {
    json!({"source_ref": SOURCE, "revision": "a".repeat(64), "slug": "start",
           "markdown": format!("# Start\n{text}\n")})
}

fn notes_conflict() -> Value {
    json!({
        "conflict_id": CONFLICT,
        "operation_id": "018f0000-0000-7000-8000-000000000d01",
        "connection_id": CONNECTION,
        "path": VAULT_PATH,
        "origin": "site:site-a",
        "media_type": "text/markdown",
        "state": "open",
        "texts": {"base": "a", "current": "b", "writer": "c"},
        "texts_over_limit": [],
        "base_hash": "a".repeat(64),
        "current_hash": "b".repeat(64),
        "writer_hash": "c".repeat(64),
    })
}

fn origin() -> String {
    format!("https://{HOSTNAME}")
}

fn edit_body(source_ref: &str, base: char, key: &str) -> Value {
    json!({
        "source_ref": source_ref,
        "base_hash": base.to_string().repeat(64),
        "content_blob": blob_ref(),
        "content_hash": content_hash(),
        "idempotency_key": key,
    })
}

async fn post_edit(edge: &Edge, body: &Value) -> (u16, Value) {
    let request = from_site(
        "POST",
        "/v1/sites/site-a/edits",
        &edge.cookie,
        true,
        Some(&origin()),
        body,
    );
    let (status, _, answer) = send(&edge.app, request).await;
    (status.as_u16(), answer)
}

async fn upload(edge: &Edge) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/sites/site-a/drafts/sources/{SOURCE}/blobs"))
        .header("cookie", format!("{COOKIE}={}", edge.cookie))
        .header("x-ratatoskr-site-request", "1")
        .header("origin", origin())
        .header("content-type", "text/markdown")
        .body(Body::from(content()))
        .expect("a request");
    let (status, _, answer) = send(&edge.app, request).await;
    (status.as_u16(), answer)
}

/// The owner's event stream of `site-a`.
async fn owner_stream(edge: &Edge) -> Body {
    let request = Request::builder()
        .uri("/v1/sites/site-a/events")
        .header("cookie", format!("{COOKIE}={}", edge.cookie))
        .body(Body::empty())
        .expect("a request");
    let response = edge.app.clone().oneshot(request).await.expect("a response");
    assert_eq!(response.status(), 200, "the owner stream");
    response.into_body()
}

/// The data of the next event of `body` within `wait`, skipping keep-alive comments.
async fn next_event(body: &mut Body, buffer: &mut String, wait: Duration) -> Option<Value> {
    loop {
        if let Some((block, rest)) = buffer.split_once("\n\n") {
            let (block, rest) = (block.to_owned(), rest.to_owned());
            *buffer = rest;
            if let Some(data) = block.lines().find_map(|line| line.strip_prefix("data:")) {
                return serde_json::from_str(data.trim()).ok();
            }
            continue;
        }
        let frame = tokio::time::timeout(wait, body.frame()).await.ok()??.ok()?;
        if let Ok(data) = frame.into_data() {
            buffer.push_str(&String::from_utf8_lossy(&data));
        }
    }
}

fn message(subject: &str, owner: Uuid, payload: &Value) -> Incoming {
    Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::parse(subject).expect("a subject"),
        producer: "ratatoskr-notes".to_owned(),
        payload: json!({
            "event_id": Uuid::now_v7(),
            "producer": "ratatoskr-notes",
            "tenant_id": format!("user:{owner}"),
            "payload": payload.clone(),
        }),
    }
}

/// `ratatoskr-notes` reports the write operation `operation` as running, then succeeded.
async fn report_success(pool: &sqlx::PgPool, edge: &Edge, operation: &str) {
    let handler = SiteEditReports {
        hub: Arc::clone(&edge.hub),
    };
    for status in ["running", "succeeded"] {
        let report = message(
            "evt.platform.operation.reported.v1",
            edge.owner,
            &json!({"operation_id": operation, "status": status, "results": [], "warnings": []}),
        );
        let now = jiff::Timestamp::now(); // wall-clock: Edge accepted the operation at real time
        platform_eventing::consumer::deliver(pool, &handler, &report, now)
            .await
            .expect("the report is applied");
    }
}

/// `ratatoskr-notes` raises the conflict [`CONFLICT`] for `operation`.
async fn raise_conflict(pool: &sqlx::PgPool, edge: &Edge, operation: &str) {
    let handler = SiteEditConflicts {
        hub: Arc::clone(&edge.hub),
    };
    let raised = message(
        "evt.notes.conflict.raised.v1",
        edge.owner,
        &json!({
            "conflict_id": CONFLICT,
            "operation_id": operation,
            "connection_id": CONNECTION,
            "path": VAULT_PATH,
            "origin": "site:site-a",
            "media_type": "text/markdown",
            "base_hash": "a".repeat(64),
            "current_hash": "b".repeat(64),
            "writer_hash": content_hash(),
        }),
    );
    let now = jiff::Timestamp::now(); // wall-clock: Edge accepted the operation at real time
    platform_eventing::consumer::deliver(pool, &handler, &raised, now)
        .await
        .expect("the conflict is applied");
}

/// Spec `site-owner-editing`, "The owner opens a published page for editing".
#[tokio::test]
async fn draft_page_returns_source_ref_and_revision() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let request = from_site(
        "GET",
        "/v1/sites/site-a/drafts/pages/start",
        &edge.cookie,
        false,
        None,
        &Value::Null,
    );
    let (status, _, body) = send(&edge.app, request).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, draft("old text"));
    let received = edge.sites.received();
    let read = received
        .iter()
        .find(|request| request.uri == "/v1/sites/site-a/drafts/pages/start")
        .expect("the sites service answers the draft");
    assert_eq!(
        read.headers
            .get("x-ratatoskr-session-kind")
            .and_then(|v| v.to_str().ok()),
        Some("site_owner"),
        "the draft read names the site owner session"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "The owner asks for a note of another publication".
#[tokio::test]
async fn out_of_scope_source_is_not_found() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let request = from_site(
        "GET",
        "/v1/sites/site-a/drafts/sources/src_other",
        &edge.cookie,
        false,
        None,
        &Value::Null,
    );
    let (status, _, body) = send(&edge.app, request).await;
    assert_eq!(
        (status.as_u16(), body["code"].as_str()),
        (404, Some("sites.source_not_found")),
        "the not-found answer of the sites service: {body}"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "Content is uploaded": the answer names a blob of notes, and no row
/// of Edge's schemas holds the content.
#[tokio::test]
async fn edit_content_never_enters_outbox_logs_or_stream() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let (status, uploaded) = upload(&edge).await;
    assert_eq!(status, 201, "{uploaded}");
    assert_eq!(uploaded["content_blob"]["owner_service"], "ratatoskr-notes");
    assert_eq!(uploaded["content_hash"], content_hash());
    let (status, answer) = post_edit(&edge, &edit_body(SOURCE, 'a', "edit-marker")).await;
    assert_eq!(status, 202, "{answer}");

    let tables: Vec<(String, String)> = sqlx::query_as(
        "select table_schema::text, table_name::text from information_schema.tables
          where table_schema in ('identity', 'operations') and table_type = 'BASE TABLE'",
    )
    .fetch_all(harness.pool())
    .await
    .expect("the tables");
    for (schema, table) in tables {
        let sql =
            format!("select count(*) from {schema}.{table} t where position($1 in t::text) > 0");
        let rows: i64 = sqlx::query_scalar(&sql)
            .bind(MARKER)
            .fetch_one(harness.pool())
            .await
            .expect("a search");
        assert_eq!(rows, 0, "{schema}.{table} holds the content of the edit");
    }

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "An edit is accepted".
#[tokio::test]
async fn edit_publishes_one_write_command_with_site_origin() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let (status, answer) = post_edit(&edge, &edit_body(SOURCE, 'a', "edit-1")).await;
    assert_eq!(status, 202, "{answer}");
    let commands = outbox(harness.pool(), WRITE).await;
    assert_eq!(commands.len(), 1, "one write command");
    let command = &commands[0]["payload"];
    assert_eq!(command["operation_id"], answer["operation_id"]);
    assert_eq!(command["connection_id"], CONNECTION);
    assert_eq!(command["origin"], "site:site-a");
    assert_eq!(
        command["operation"],
        json!({"update": {"path": VAULT_PATH, "base_hash": "a".repeat(64), "blob": blob_ref()}})
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "The edit completes only when the vault has it".
#[tokio::test]
async fn edit_completed_follows_operation_success() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;
    let mut stream = owner_stream(&edge).await;
    let mut buffer = String::new();

    let (status, answer) = post_edit(&edge, &edit_body(SOURCE, 'a', "edit-2")).await;
    assert_eq!(status, 202, "{answer}");
    let operation = answer["operation_id"].as_str().unwrap().to_owned();
    assert_eq!(
        next_event(&mut stream, &mut buffer, Duration::from_millis(300)).await,
        None,
        "an event before the vault has the write"
    );
    report_success(harness.pool(), &edge, &operation).await;
    assert_eq!(
        next_event(&mut stream, &mut buffer, Duration::from_secs(5)).await,
        Some(json!({"type": "edit.completed", "payload": {
            "operation_id": operation, "source_ref": SOURCE, "revision": content_hash()}}))
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "An edit outside the publications".
#[tokio::test]
async fn out_of_scope_edit_is_not_found() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let (status, answer) = post_edit(&edge, &edit_body("src_other", 'a', "edit-3")).await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (404, Some("platform.resource.not_found")),
        "{answer}"
    );
    assert!(
        outbox(harness.pool(), WRITE).await.is_empty(),
        "a command was written"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "A retried edit".
#[tokio::test]
async fn same_idempotency_key_same_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let body = edit_body(SOURCE, 'a', "edit-retried");
    let (first_status, first) = post_edit(&edge, &body).await;
    let (second_status, second) = post_edit(&edge, &body).await;
    assert_eq!(
        (first_status, second_status),
        (202, 202),
        "{first} {second}"
    );
    assert_eq!(first["operation_id"], second["operation_id"]);
    assert_eq!(
        outbox(harness.pool(), WRITE).await.len(),
        1,
        "one write command"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "The note changed in Obsidian during the edit".
#[tokio::test]
async fn conflict_emits_edit_conflicted() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;
    let mut stream = owner_stream(&edge).await;
    let mut buffer = String::new();

    let (status, answer) = post_edit(&edge, &edit_body(SOURCE, 'a', "edit-4")).await;
    assert_eq!(status, 202, "{answer}");
    let operation = answer["operation_id"].as_str().unwrap().to_owned();
    raise_conflict(harness.pool(), &edge, &operation).await;
    assert_eq!(
        next_event(&mut stream, &mut buffer, Duration::from_secs(5)).await,
        Some(json!({"type": "edit.conflicted", "payload": {
            "operation_id": operation, "conflict_id": CONFLICT, "source_ref": SOURCE}}))
    );
    let request = from_site(
        "GET",
        &format!("/v1/sites/site-a/conflicts/{CONFLICT}"),
        &edge.cookie,
        false,
        None,
        &Value::Null,
    );
    let (status, _, conflict) = send(&edge.app, request).await;
    assert_eq!(status, 200, "{conflict}");
    assert_eq!(conflict["source_ref"], SOURCE);
    assert_eq!(conflict["texts"]["current"], "b");
    assert!(
        conflict.get("path").is_none() && conflict.get("connection_id").is_none(),
        "{conflict}"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "The owner resolves the conflict".
#[tokio::test]
async fn new_edit_on_current_revision_resolves_conflict() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;
    let mut stream = owner_stream(&edge).await;
    let mut buffer = String::new();

    let (_, first) = post_edit(&edge, &edit_body(SOURCE, 'a', "edit-5")).await;
    raise_conflict(
        harness.pool(),
        &edge,
        first["operation_id"].as_str().unwrap(),
    )
    .await;
    let conflicted = next_event(&mut stream, &mut buffer, Duration::from_secs(5)).await;
    assert_eq!(
        conflicted.as_ref().map(|event| event["type"].clone()),
        Some(json!("edit.conflicted"))
    );

    let (status, second) = post_edit(&edge, &edit_body(SOURCE, 'b', "edit-6")).await;
    assert_eq!(status, 202, "{second}");
    let operation = second["operation_id"].as_str().unwrap().to_owned();
    report_success(harness.pool(), &edge, &operation).await;
    let completed = next_event(&mut stream, &mut buffer, Duration::from_secs(5))
        .await
        .expect("an event");
    assert_eq!(completed["type"], "edit.completed");
    assert_eq!(completed["payload"]["operation_id"], operation);

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "An edit in a manual publication is saved but not released": Edge
/// starts no release, the anonymous page is the previous release, and the owner reads the draft.
#[tokio::test]
async fn manual_edit_does_not_start_a_release() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let (_, answer) = post_edit(&edge, &edit_body(SOURCE, 'a', "edit-7")).await;
    report_success(
        harness.pool(),
        &edge,
        answer["operation_id"].as_str().unwrap(),
    )
    .await;
    assert!(
        outbox(harness.pool(), BUILD).await.is_empty(),
        "an edit started a release"
    );
    let request = Request::builder()
        .uri("/v1/sites/site-a/content/pages/start")
        .body(Body::empty())
        .expect("a request");
    let (status, _, page) = send(&edge.app, request).await;
    assert_eq!(
        (status.as_u16(), page["text"].as_str()),
        (200, Some("old text")),
        "{page}"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "An edit in an automatic publication": Edge publishes no build
/// command for an edit; the sites service starts the coalesced build from the file change.
#[tokio::test]
async fn automatic_edit_publishes_no_build_command() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let (status, answer) = post_edit(&edge, &edit_body(SOURCE, 'a', "edit-8")).await;
    assert_eq!(status, 202, "{answer}");
    report_success(
        harness.pool(),
        &edge,
        answer["operation_id"].as_str().unwrap(),
    )
    .await;
    assert!(
        outbox(harness.pool(), BUILD).await.is_empty(),
        "an edit published a build command"
    );
    assert_eq!(outbox(harness.pool(), WRITE).await.len(), 1);

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, "The owner publishes a ... publication from the site".
#[tokio::test]
async fn release_request_publishes_one_build_command_in_both_modes() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let request = from_site(
        "POST",
        "/v1/sites/site-a/releases",
        &edge.cookie,
        true,
        Some(&origin()),
        &Value::Null,
    );
    let (status, _, answer) = send(&edge.app, request).await;
    assert_eq!(status, 202, "{answer}");
    assert!(answer["operation_id"].is_string(), "{answer}");
    assert_eq!(
        outbox(harness.pool(), BUILD).await.len(),
        1,
        "one build command"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Spec `site-owner-editing`, design D6: the upload reaches notes as the site owner session with the
/// origin of the site, never as a primary session.
#[tokio::test]
async fn site_upload_claim_is_not_relabelled_as_primary() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;

    let (status, uploaded) = upload(&edge).await;
    assert_eq!(status, 201, "{uploaded}");
    let received = edge.notes.received();
    assert!(!received.is_empty(), "notes received the upload");
    for request in &received {
        let header = |name: &str| request.headers.get(name).and_then(|v| v.to_str().ok());
        assert_eq!(
            header("x-ratatoskr-session-kind"),
            Some("site_owner"),
            "{}",
            request.uri
        );
        assert_eq!(
            header("x-ratatoskr-origin"),
            Some("site:site-a"),
            "{}",
            request.uri
        );
        assert_eq!(
            header("x-ratatoskr-user-id"),
            Some(edge.owner.to_string().as_str())
        );
    }
    let first = &received[0];
    assert_eq!(
        first.uri,
        format!("/v1/notes/connections/{CONNECTION}/blobs")
    );
    let declared: Value = serde_json::from_slice(&first.body).expect("a session request");
    assert_eq!(declared["digest"]["hex"], content_hash());
    assert!(
        received.iter().all(
            |request| !String::from_utf8_lossy(&request.body).contains(MARKER)
                || request.uri.ends_with("/chunks/0")
        ),
        "the content goes only into the chunk"
    );

    harness.cleanup().await.expect("cleanup");
}

/// Review M1 of SITE-027: the row of an edit commits in the transaction of its operation and its
/// command. When the row cannot be written, nothing is admitted, and a retry with the same key is
/// a clean first attempt.
#[tokio::test]
async fn failed_edit_record_admits_nothing() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;
    for statement in [
        "create function operations.refuse_site_edit() returns trigger language plpgsql
           as $$ begin raise exception 'the site edit is refused'; end $$",
        "create trigger refuse_site_edit before insert on operations.site_edits
           for each row execute function operations.refuse_site_edit()",
    ] {
        sqlx::query(statement)
            .execute(harness.pool())
            .await
            .expect("the fault");
    }

    let body = edit_body(SOURCE, 'a', "edit-m1");
    let (status, answer) = post_edit(&edge, &body).await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (504, Some("platform.request.timeout")),
        "{answer}"
    );
    assert!(
        outbox(harness.pool(), WRITE).await.is_empty(),
        "a write command was admitted without its edit row"
    );
    let operations: i64 = sqlx::query_scalar(
        "select count(*) from operations.operations where kind = 'notes.file.write'",
    )
    .fetch_one(harness.pool())
    .await
    .expect("a count");
    assert_eq!(
        operations, 0,
        "an operation was admitted without its edit row"
    );

    sqlx::query("drop trigger refuse_site_edit on operations.site_edits")
        .execute(harness.pool())
        .await
        .expect("the fault is removed");
    let (status, answer) = post_edit(&edge, &body).await;
    assert_eq!(status, 202, "{answer}");
    let rows: i64 = sqlx::query_scalar("select count(*) from operations.site_edits")
        .fetch_one(harness.pool())
        .await
        .expect("a count");
    assert_eq!((rows, outbox(harness.pool(), WRITE).await.len()), (1, 1));

    harness.cleanup().await.expect("cleanup");
}

/// Review L9 of SITE-027: only content over the bound is 413; a body that breaks off is an
/// invalid request, and neither reaches notes as a chunk.
#[tokio::test]
async fn only_oversized_content_is_too_large() {
    let harness = TestDatabase::create().await.expect("a test database");
    let edge = edge(&harness).await;
    let blobs = format!("/v1/sites/site-a/drafts/sources/{SOURCE}/blobs");
    let send_body = |body: Body| {
        Request::builder()
            .method("POST")
            .uri(&blobs)
            .header("cookie", format!("{COOKIE}={}", edge.cookie))
            .header("x-ratatoskr-site-request", "1")
            .header("origin", origin())
            .header("content-type", "text/markdown")
            .body(body)
            .expect("a request")
    };

    let broken = futures_util::stream::iter(vec![
        Ok(axum::body::Bytes::from_static(b"# Start\n")),
        Err(std::io::Error::other("the client went away")),
    ]);
    let (status, _, answer) = send(&edge.app, send_body(Body::from_stream(broken))).await;
    assert_eq!(
        (status.as_u16(), answer["code"].as_str()),
        (400, Some("platform.request.invalid")),
        "a broken body: {answer}"
    );
    let oversized = vec![b'a'; 16 * 1024 * 1024 + 1];
    let (status, _, answer) = send(&edge.app, send_body(Body::from(oversized))).await;
    assert_eq!(status.as_u16(), 413, "content over 16 MiB: {answer}");
    assert!(
        edge.notes
            .received()
            .iter()
            .all(|request| !request.uri.contains("/chunks/")),
        "a chunk reached notes"
    );

    harness.cleanup().await.expect("cleanup");
}
