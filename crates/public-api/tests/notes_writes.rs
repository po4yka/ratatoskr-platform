//! The write routes of `ratatoskr-notes` at Edge (workspace change `add-notes-write-back`, task
//! 10.1): a write is an idempotent operation whose command names its content by a `BlobRef` and
//! never carries it, and the write-mode switch is proxied in the class `control`.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

mod notes_support;

use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use http::{Request, StatusCode};
use notes_support::{
    OWNER_CREDENTIAL, app, device_credential, notes_route, now, seed_owner, seed_plugin, send,
    stub, values,
};
use platform_api_doc::Method;
use platform_eventing::Subject;
use platform_eventing::publisher::{PublishError, Publisher};
use platform_persistence::test_support::TestDatabase;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

const CONNECTION: &str = "018f0000-0000-7000-8000-0000000000c1";
const BASE_HASH: &str = "0a1b2c3d4e5f60710a1b2c3d4e5f60710a1b2c3d4e5f60710a1b2c3d4e5f6071";

fn writes_uri() -> String {
    format!("/v1/notes/connections/{CONNECTION}/writes")
}

fn write_mode_uri() -> String {
    format!("/v1/notes/connections/{CONNECTION}/write-mode")
}

/// A `BlobRef` owned by `ratatoskr-notes` that names `bytes`.
fn blob_of(bytes: &[u8], media_type: &str) -> serde_json::Value {
    serde_json::json!({
        "owner_service": "ratatoskr-notes",
        "digest": { "algorithm": "sha256", "hex": format!("{:x}", Sha256::digest(bytes)) },
        "media_type": media_type,
        "length_bytes": bytes.len(),
    })
}

/// An `update` of `Projects/Plan.md` to `bytes`.
fn update_body(bytes: &[u8]) -> String {
    serde_json::json!({
        "operation": {
            "update": {
                "path": "Projects/Plan.md",
                "base_hash": BASE_HASH,
                "blob": blob_of(bytes, "text/markdown"),
            }
        }
    })
    .to_string()
}

/// An owner request to `uri` with `body`, and the `Idempotency-Key` when there is one.
fn owner_request(method: &str, uri: &str, key: Option<&str>, body: String) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {OWNER_CREDENTIAL}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    builder.body(Body::from(body)).expect("a request")
}

async fn operations_of(pool: &sqlx::PgPool, owner: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from operations.operations where owner_user_id = $1")
        .bind(owner)
        .fetch_one(pool)
        .await
        .expect("count the operations")
}

async fn outbox_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from operations.outbox")
        .fetch_one(pool)
        .await
        .expect("count the outbox rows")
}

/// A publisher that keeps every envelope that the outbox pump hands to the broker.
#[derive(Default)]
struct Recorder {
    published: Mutex<Vec<(String, Vec<u8>)>>,
}

impl Publisher for Recorder {
    async fn publish(
        &self,
        subject: &Subject,
        payload: &[u8],
        _message_id: &str,
    ) -> Result<(), PublishError> {
        self.published
            .lock()
            .expect("the recorder lock")
            .push((subject.to_string(), payload.to_vec()));
        Ok(())
    }
}

#[tokio::test]
async fn write_requires_idempotency_key() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);

    let (status, body) = send(
        &app,
        owner_request("POST", &writes_uri(), None, update_body(b"# Plan\n")),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_key_required");
    assert_eq!(operations_of(harness.pool(), owner).await, 0);
    assert_eq!(outbox_rows(harness.pool()).await, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), seen.recv())
            .await
            .is_err(),
        "a write reached the notes service"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn invalid_body_is_refused() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let markdown = blob_of(b"# Plan\n", "text/markdown");
    let foreign = serde_json::json!({
        "owner_service": "ratatoskr-telegram",
        "digest": markdown["digest"],
        "media_type": "text/markdown",
        "length_bytes": 7,
    });
    let update = |path: &str, blob: &serde_json::Value| serde_json::json!({ "update": { "path": path, "base_hash": BASE_HASH, "blob": blob } });
    let bodies = [
        // Not JSON.
        "{".to_owned(),
        // The file text inline instead of a `BlobRef`.
        serde_json::json!({ "operation": { "update": {
            "path": "Projects/Plan.md", "base_hash": BASE_HASH, "blob": "# Plan\n",
        } } })
        .to_string(),
        // Two operations in one write.
        serde_json::json!({ "operation": {
            "update": update("Projects/Plan.md", &markdown)["update"],
            "delete": { "path": "Projects/Plan.md", "base_hash": BASE_HASH },
        } })
        .to_string(),
        // A path that leaves the vault, and one in the vault configuration.
        serde_json::json!({ "operation": update("../secrets.md", &markdown) }).to_string(),
        serde_json::json!({ "operation": update(".obsidian/app.json", &markdown) }).to_string(),
        // Portable write names: both source and destination use the contract path type.
        serde_json::json!({ "operation": update("Notes./x.md", &markdown) }).to_string(),
        serde_json::json!({ "operation": update(".obsidian./app.json", &markdown) }).to_string(),
        serde_json::json!({ "operation": update("Notes /x.md", &markdown) }).to_string(),
        serde_json::json!({ "operation": update("A:B.md", &markdown) }).to_string(),
        serde_json::json!({ "operation": { "rename": {
            "path": "Projects/Plan.md", "base_hash": BASE_HASH, "target_path": "Notes/B:C.md",
        } } })
        .to_string(),
        serde_json::json!({ "operation": { "rename": {
            "path": "Projects/Plan.md", "base_hash": BASE_HASH, "target_path": "Notes./Plan.md",
        } } })
        .to_string(),
        serde_json::json!({ "operation": { "rename": {
            "path": "Projects/Plan.md", "base_hash": BASE_HASH, "target_path": "Notes /Plan.md",
        } } })
        .to_string(),
        // An update without its base.
        serde_json::json!({ "operation": { "update": {
            "path": "Projects/Plan.md", "blob": markdown,
        } } })
        .to_string(),
        // Content that another service owns.
        serde_json::json!({ "operation": update("Projects/Plan.md", &foreign) }).to_string(),
        // A frontmatter value that is not JSON.
        serde_json::json!({ "operation": { "set_property": {
            "path": "Projects/Plan.md", "base_hash": BASE_HASH, "property_key": "status",
            "property_change": { "set": markdown },
        } } })
        .to_string(),
        // A member that the request does not declare: the origin is Edge's to set.
        serde_json::json!({
            "operation": update("Projects/Plan.md", &markdown),
            "origin": "site:docs",
        })
        .to_string(),
    ];

    for (index, body) in bodies.into_iter().enumerate() {
        let key = format!("invalid-{index}");
        let (status, answer) =
            send(&app, owner_request("POST", &writes_uri(), Some(&key), body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body {index}: {answer}");
        assert_eq!(answer["code"], "platform.request.invalid", "body {index}");
    }

    assert_eq!(operations_of(harness.pool(), owner).await, 0);
    assert_eq!(outbox_rows(harness.pool()).await, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), seen.recv())
            .await
            .is_err(),
        "an invalid write reached the notes service"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn write_command_carries_no_file_bytes() {
    let (address, _seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let marker = format!("marker-{}", Uuid::now_v7().simple());
    let note = format!("# Plan\n\nThe private line {marker}.\n");

    let (status, body) = send(
        &app,
        owner_request(
            "POST",
            &writes_uri(),
            Some("write-1"),
            update_body(note.as_bytes()),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let operation_id: Uuid = body["operation_id"]
        .as_str()
        .expect("an operation id")
        .parse()
        .expect("a UUID");
    let kind: String = sqlx::query_scalar(
        "select kind from operations.operations where operation_id = $1 and owner_user_id = $2",
    )
    .bind(operation_id)
    .bind(owner)
    .fetch_one(harness.pool())
    .await
    .expect("the write operation");
    assert_eq!(kind, "notes.file.write");

    let row: String =
        sqlx::query_scalar("select payload::text from operations.outbox where operation_id = $1")
            .bind(operation_id)
            .fetch_one(harness.pool())
            .await
            .expect("one outbox row for the write");
    assert!(!row.contains(&marker), "the outbox row holds the note");

    let recorder = Recorder::default();
    let report = platform_eventing::pump::run_once(
        harness.pool(),
        &recorder,
        "notes-writes-test",
        10,
        now(),
    )
    .await
    .expect("the pump runs");
    assert_eq!(report.published, 1);
    let published = recorder.published.into_inner().expect("the recorder lock");
    let (subject, envelope) = &published[0];
    assert_eq!(subject, "cmd.notes.file.write_requested.v1");
    assert!(
        !String::from_utf8_lossy(envelope).contains(&marker),
        "the published envelope holds the note"
    );

    let envelope: serde_json::Value =
        serde_json::from_slice(envelope).expect("the envelope is JSON");
    assert_eq!(envelope["command_type"], "notes.file.write_requested.v1");
    assert_eq!(envelope["producer"], "ratatoskr-platform");
    assert_eq!(
        envelope["aggregate_id"],
        format!("operation:{operation_id}")
    );
    assert_eq!(envelope["tenant_id"], format!("user:{owner}"));
    let payload = &envelope["payload"];
    assert_eq!(payload["operation_id"], operation_id.to_string());
    assert_eq!(payload["connection_id"], CONNECTION);
    assert_eq!(payload["origin"], "platform");
    assert_eq!(
        payload["idempotency_key"]["hex"],
        format!("{:x}", Sha256::digest(b"write-1"))
    );
    assert_eq!(
        payload["operation"]["update"]["blob"],
        blob_of(note.as_bytes(), "text/markdown")
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// The plugin and the export agent are channels of a vault. Their device credential keeps to the
/// device routes, so it neither writes nor changes the write mode.
#[tokio::test]
async fn device_cannot_write_or_change_the_write_mode() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let (owner, device) = seed_plugin(harness.pool()).await;
    let app = app(&harness, &gateway);
    let credential = device_credential(&app, device).await;
    let device_request = |method: &str, uri: &str, body: String| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .header("idempotency-key", "device-1")
            .body(Body::from(body))
            .expect("a request")
    };

    for request in [
        device_request("POST", &writes_uri(), update_body(b"# Plan\n")),
        device_request(
            "PUT",
            &write_mode_uri(),
            r#"{"write_mode":"on"}"#.to_owned(),
        ),
    ] {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }

    assert_eq!(operations_of(harness.pool(), owner).await, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), seen.recv())
            .await
            .is_err(),
        "a device request reached the notes service"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn write_mode_route_is_control_class() {
    let (address, mut seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let on = || r#"{"write_mode":"on"}"#.to_owned();

    let (status, body) = send(&app, owner_request("PUT", &write_mode_uri(), None, on())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_key_required");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), seen.recv())
            .await
            .is_err(),
        "a write-mode change without a key reached the notes service"
    );

    // The control budget of the test route is 1 KiB and its transfer budget 1 MiB.
    let padded = format!(r#"{{"write_mode":"on","padding":"{}"}}"#, "x".repeat(2048));
    let (status, body) = send(
        &app,
        owner_request("PUT", &write_mode_uri(), Some("mode-big"), padded),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");

    let (status, body) = send(
        &app,
        owner_request("PUT", &write_mode_uri(), Some("mode-1"), on()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let operation_id = body["operation_id"].as_str().expect("an operation id");
    let kind: String =
        sqlx::query_scalar("select kind from operations.operations where operation_id = $1")
            .bind(Uuid::parse_str(operation_id).expect("a UUID"))
            .fetch_one(harness.pool())
            .await
            .expect("the write-mode operation");
    assert_eq!(kind, "notes.connection.write_mode");
    let forwarded = seen.recv().await.expect("the change reaches notes");
    assert_eq!(forwarded.method, http::Method::PUT);
    assert_eq!(forwarded.path, write_mode_uri());
    assert_eq!(&forwarded.body[..], on().as_bytes());
    assert_eq!(
        values(&forwarded.headers, "x-ratatoskr-operation-id"),
        [operation_id]
    );
    assert_eq!(
        values(&forwarded.headers, "x-ratatoskr-user-id"),
        [owner.to_string()]
    );
    assert_eq!(operations_of(harness.pool(), owner).await, 1);

    let route = platform_public_api::surface()
        .routes
        .into_iter()
        .find(|route| {
            route.method == Method::Put
                && route.path == "/v1/notes/connections/{connection_id}/write-mode"
        })
        .expect("the write-mode route is in the OpenAPI document");
    assert!(
        route
            .parameters
            .iter()
            .any(|parameter| parameter.name == "Idempotency-Key" && parameter.required),
        "the write-mode route does not require an Idempotency-Key"
    );
    task.abort();
    harness.cleanup().await.expect("cleanup");
}

/// A write and a resolution are both `notes.file.write`, and notes deduplicates a write by its key
/// and connection. A key that one route took is therefore refused on the other with `409`, instead
/// of a second operation whose command notes would refuse as a reused key. The same request with
/// the same key still replays its first operation.
#[tokio::test]
async fn a_key_is_one_write_across_the_write_and_resolution_routes() {
    let (address, _seen, task) = stub().await;
    let gateway = notes_route(address);
    let harness = TestDatabase::create().await.expect("a test database");
    let owner = seed_owner(harness.pool()).await;
    let app = app(&harness, &gateway);
    let resolution_uri = "/v1/notes/conflicts/018f0000-0000-7000-8000-0000000000e1/resolution";
    let resolution =
        || serde_json::json!({ "connection_id": CONNECTION, "choice": "keep_current" }).to_string();

    let (status, first) = send(
        &app,
        owner_request(
            "POST",
            &writes_uri(),
            Some("key-1"),
            update_body(b"# Plan\n"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, body) = send(
        &app,
        owner_request("POST", resolution_uri, Some("key-1"), resolution()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "platform.request.idempotency_conflict");

    let (status, resolved) = send(
        &app,
        owner_request("POST", resolution_uri, Some("key-2"), resolution()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resolved}");
    let (status, body) = send(
        &app,
        owner_request(
            "POST",
            &writes_uri(),
            Some("key-2"),
            update_body(b"# Plan\n"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, replayed) = send(
        &app,
        owner_request("POST", resolution_uri, Some("key-2"), resolution()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{replayed}");
    assert_eq!(replayed["operation_id"], resolved["operation_id"]);

    assert_eq!(operations_of(harness.pool(), owner).await, 2);
    assert_eq!(outbox_rows(harness.pool()).await, 2);
    task.abort();
    harness.cleanup().await.expect("cleanup");
}
