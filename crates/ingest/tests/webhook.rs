//! The generic webhook adapter — tests I-1 … I-9.
//!
//! `ARCHITECTURE.md` S9 lists six steps and these cover all six: who may push, what a redelivery
//! does, what a shape violation does, where a signal is routed, and what reaches the outbox.
//!
//! Every test runs through the real public pipeline rather than the bare router, because the
//! middleware is what renders an authored failure into an `ErrorEnvelope`; a test on the router
//! alone would assert statuses and prove nothing about bodies.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use platform_core::RuntimeRole;
use platform_core::config::PublicConfig;
use platform_http::HttpState;
use platform_identity::SecretDigest;
use platform_ingest::{IngestState, Target};
use platform_persistence::test_support::TestDatabase;
use sqlx::Row as _;
use tower::ServiceExt as _;
use uuid::Uuid;

const TOKEN: &str = "webhook-source-credential-00000000";
const SIGNAL: &str = r#"{"url":"https://example.test/feed/1"}"#;

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn app_with(harness: &TestDatabase, per_minute: u32) -> Router {
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 15,
        max_body_bytes: 1_048_576,
        max_concurrent_requests: 64,
        actor_requests_per_minute: per_minute,
    };
    let mut state = IngestState::new(harness.database.clone());
    state.actor_limit = Arc::new(platform_http::ActorLimiter::new(per_minute));
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Ingest)),
        &config,
        platform_ingest::routes(state),
    )
}

fn app(harness: &TestDatabase) -> Router {
    let config = PublicConfig {
        bind: "127.0.0.1:0".parse().expect("a socket address"),
        request_timeout_seconds: 15,
        max_body_bytes: 1_048_576,
        max_concurrent_requests: 64,
        actor_requests_per_minute: 120,
    };
    platform_http::observe::public_router(
        Arc::new(HttpState::new(RuntimeRole::Ingest)),
        &config,
        platform_ingest::routes(IngestState::new(harness.database.clone())),
    )
}

/// A user, and a source they own that presents `token`.
async fn seed(pool: &sqlx::PgPool, token: &str) -> (Uuid, Uuid) {
    let user = platform_identity::user::create_user(pool, now())
        .await
        .expect("a user");
    let source = platform_ingest::source::register(
        pool,
        user.user_id,
        "a test source",
        SecretDigest::of(token),
        Target::ContentCapture,
        now(),
    )
    .await
    .expect("a source");
    (user.user_id, source)
}

fn push(source: Uuid, token: Option<&str>, key: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/v1/ingest/webhooks/{source}"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// I-1. The whole of S9 in one request: authenticated, deduplicated, normalized, routed, published
/// as a command, and readable as an operation.
#[tokio::test]
async fn a_signal_becomes_one_operation_and_one_command() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, source) = seed(pool, TOKEN).await;
    let app = app(&harness);

    let (status, body) = send(&app, push(source, Some(TOKEN), Some("delivery-1"), SIGNAL)).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "accepted");
    let operation_id: Uuid = body["operation_id"]
        .as_str()
        .expect("an operation id")
        .parse()
        .expect("a uuid");

    // The operation belongs to the user who owns the source, not to the source: a source has no
    // session and cannot read it, and the owner can.
    let operation = platform_operations::find(pool, operation_id)
        .await
        .expect("a read")
        .expect("an operation");
    assert_eq!(operation.owner_user_id, owner);
    assert_eq!(operation.kind, Target::ContentCapture.operation_kind());

    // Exactly one command, on the subject the extractor subscribes to, carrying the address and
    // nothing the source could have chosen the shape of.
    let rows =
        sqlx::query("select subject, payload from operations.outbox where operation_id = $1")
            .bind(operation_id)
            .fetch_all(pool)
            .await
            .expect("the outbox");
    assert_eq!(rows.len(), 1);
    let subject: String = rows[0].try_get("subject").expect("a subject");
    assert_eq!(subject, "cmd.content.capture.requested.v1");
    let payload: serde_json::Value = rows[0].try_get("payload").expect("a payload");
    assert_eq!(payload["command_type"], "content.capture.requested.v1");
    assert_eq!(payload["operation_id"], operation_id.to_string());
    assert_eq!(payload["tenant_id"], format!("user:{owner}"));
    assert_eq!(payload["payload"]["url"], "https://example.test/feed/1");
}

/// I-2. The command a webhook produces is indistinguishable from the one the client route produces.
///
/// This is the whole justification for routing a webhook into an existing bounded context rather
/// than inventing an ingest-shaped command: a consumer must not be able to tell, and therefore
/// cannot come to depend on, which door a request came through.
#[tokio::test]
async fn the_command_is_the_same_shape_the_client_route_emits() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (_, source) = seed(pool, TOKEN).await;
    let app = app(&harness);

    let (_, body) = send(&app, push(source, Some(TOKEN), Some("delivery-1"), SIGNAL)).await;
    let operation_id = body["operation_id"].as_str().expect("an id");

    let payload: serde_json::Value =
        sqlx::query_scalar("select payload from operations.outbox where operation_id = $1::uuid")
            .bind(operation_id)
            .fetch_one(pool)
            .await
            .expect("a payload");

    let mut members: Vec<&str> = payload
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    members.sort_unstable();
    assert_eq!(
        members,
        [
            "command_id",
            "command_type",
            "correlation_id",
            "idempotency_key",
            "operation_id",
            "payload",
            "requested_at",
            "tenant_id",
        ],
        "ARCHITECTURE.md S5.3 fixes these members: {payload}"
    );
}

/// I-3. Redelivery is ordinary traffic. The same identifier and the same body return the operation
/// the first delivery created — which is what stops an at-least-once webhook doing the work twice.
#[tokio::test]
async fn a_redelivery_returns_the_original_operation() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (_, source) = seed(pool, TOKEN).await;
    let app = app(&harness);

    let (first_status, first) = send(&app, push(source, Some(TOKEN), Some("d-1"), SIGNAL)).await;
    let (second_status, second) = send(&app, push(source, Some(TOKEN), Some("d-1"), SIGNAL)).await;

    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(second_status, StatusCode::ACCEPTED);
    assert_eq!(first["operation_id"], second["operation_id"]);

    let commands: i64 = sqlx::query_scalar("select count(*) from operations.outbox")
        .fetch_one(pool)
        .await
        .expect("a count");
    assert_eq!(
        commands, 1,
        "a redelivery must not enqueue a second command"
    );
}

/// I-4. The same identifier with a different body is refused, because honouring it would silently
/// replace the meaning of a signal already accepted.
#[tokio::test]
async fn the_same_identifier_with_a_different_body_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (_, source) = seed(harness.pool(), TOKEN).await;
    let app = app(&harness);

    send(&app, push(source, Some(TOKEN), Some("d-1"), SIGNAL)).await;
    let (status, body) = send(
        &app,
        push(
            source,
            Some(TOKEN),
            Some("d-1"),
            r#"{"url":"https://example.test/other"}"#,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "platform.request.idempotency_conflict");
}

/// I-5. Two sources owned by one user do not collide on a shared identifier.
///
/// The reason the source is folded into the deduplication key. `1`, `42` and a Unix second are all
/// identifiers a provider might choose, and two providers choose them without consulting each
/// other; without the fold, the second source's first delivery would be answered with the first
/// source's operation.
#[tokio::test]
async fn two_sources_of_one_user_do_not_share_an_identifier_space() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, first) = seed(pool, TOKEN).await;
    let second = platform_ingest::source::register(
        pool,
        owner,
        "the other source",
        SecretDigest::of("another-source-credential-0000000"),
        Target::ContentCapture,
        now(),
    )
    .await
    .expect("a second source");
    let app = app(&harness);

    let (_, a) = send(&app, push(first, Some(TOKEN), Some("1"), SIGNAL)).await;
    let (status, b) = send(
        &app,
        push(
            second,
            Some("another-source-credential-0000000"),
            Some("1"),
            SIGNAL,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_ne!(
        a["operation_id"], b["operation_id"],
        "each source has its own identifier space"
    );
}

/// I-6. Every way of failing to be a known source produces the same refusal.
///
/// A missing credential, an unknown one, a disabled source and a credential used against another
/// source's URL must be indistinguishable from outside, or the difference is an oracle for which
/// sources exist (`ARCHITECTURE.md` S15).
#[tokio::test]
async fn every_authentication_failure_is_the_same_refusal() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (owner, source) = seed(pool, TOKEN).await;
    let disabled = platform_ingest::source::register(
        pool,
        owner,
        "a retired source",
        SecretDigest::of("retired-source-credential-000000"),
        Target::ContentCapture,
        now(),
    )
    .await
    .expect("a source");
    sqlx::query(
        "update platform_ingest.webhook_sources set disabled_at = now() where source_id = $1",
    )
    .bind(disabled)
    .execute(pool)
    .await
    .expect("a disable");
    let app = app(&harness);

    let attempts = [
        ("no credential", push(source, None, Some("k"), SIGNAL)),
        (
            "an unknown credential",
            push(source, Some("not-a-credential"), Some("k"), SIGNAL),
        ),
        (
            "a disabled source",
            push(
                disabled,
                Some("retired-source-credential-000000"),
                Some("k"),
                SIGNAL,
            ),
        ),
        (
            "another source's URL",
            push(disabled, Some(TOKEN), Some("k"), SIGNAL),
        ),
    ];

    for (what, request) in attempts {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{what}: {body}");
        assert_eq!(body["code"], "platform.auth.unauthenticated", "{what}");
    }
}

/// I-7. Normalization is a bound, not a suggestion. Nothing that fails it reaches the outbox.
#[tokio::test]
async fn an_unusable_signal_is_refused_and_writes_nothing() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (_, source) = seed(pool, TOKEN).await;
    let app = app(&harness);

    let refused = [
        ("not JSON at all", "<rss><item/></rss>"),
        ("no url member", r#"{"link":"https://example.test/a"}"#),
        (
            "a scheme we will not fetch",
            r#"{"url":"file:///etc/passwd"}"#,
        ),
        ("no host", r#"{"url":"http://"}"#),
        ("not an address at all", r#"{"url":"example.test/a"}"#),
    ];

    for (what, body) in refused {
        let (status, envelope) = send(&app, push(source, Some(TOKEN), Some(what), body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {envelope}");
        assert_eq!(envelope["code"], "platform.request.invalid", "{what}");
    }

    let written: i64 = sqlx::query_scalar("select count(*) from operations.operations")
        .fetch_one(pool)
        .await
        .expect("a count");
    assert_eq!(written, 0, "a refused signal creates no operation");
}

/// I-8. `INTERFACES.md` requires the header on a replayable mutation, and a webhook is the most
/// replayable mutation there is.
#[tokio::test]
async fn a_signal_without_an_identifier_is_refused() {
    let harness = TestDatabase::create().await.expect("a test database");
    let (_, source) = seed(harness.pool(), TOKEN).await;
    let app = app(&harness);

    let (status, body) = send(&app, push(source, Some(TOKEN), None, SIGNAL)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "platform.request.idempotency_key_required");
}

/// I-9. A source may only be routed somewhere this build serves.
///
/// The column is data so a second source can route elsewhere without a deployment; the closed Rust
/// list is what stops a row inventing a command family nothing subscribes to. A row that names one
/// anyway is our misconfiguration, and the source is told so rather than being told its credential
/// is wrong.
#[tokio::test]
async fn a_source_routed_nowhere_is_our_failure_not_the_sources() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let (_, source) = seed(pool, TOKEN).await;
    sqlx::query("update platform_ingest.webhook_sources set target = 'weather.forecast' where source_id = $1")
        .bind(source)
        .execute(pool)
        .await
        .expect("a retarget");
    let app = app(&harness);

    let (status, body) = send(&app, push(source, Some(TOKEN), Some("k"), SIGNAL)).await;

    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "the credential was valid: {body}"
    );
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(Target::parse("weather.forecast").is_none());
}

/// I-10. Every path the generated document promises is a path this router actually serves.
///
/// The companion of P-8 in the client-facing crate: the route table produces both the router and
/// the document, and this proves the table's paths are the strings `axum` matches on.
#[tokio::test]
async fn every_documented_path_is_served() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(&harness);

    for route in platform_ingest::surface().routes {
        let path = route
            .path
            .replace("{source_id}", "01a018ae-b4e5-7f90-a17f-1e60c8ce61be");
        let method = match route.method {
            platform_api_doc::Method::Get => "GET",
            platform_api_doc::Method::Post => "POST",
            platform_api_doc::Method::Put => "PUT",
            platform_api_doc::Method::Delete => "DELETE",
        };
        let request = Request::builder()
            .method(method)
            .uri(&path)
            .body(Body::empty())
            .expect("a request");

        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {body}");
        assert_eq!(
            body["code"], "platform.auth.unauthenticated",
            "{method} {path}"
        );
    }
}

/// I-9. The audit trail, on the process with the largest unauthenticated attack surface.
///
/// Three claims, and the third is the one worth stating: an accepted signal is recorded, a
/// credential presented at ANOTHER source's URL is recorded as a denial — that is attributable, and
/// the owner of the credential is who needs to know — and an unknown credential is recorded nowhere.
/// The last is deliberate rather than an omission: an anonymous attempt has no actor to attribute,
/// and a row per attempt is write amplification an unauthenticated caller controls.
#[tokio::test]
async fn an_accepted_signal_and_an_attributable_refusal_are_audited() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app(&harness);
    let (owner, source) = seed(harness.pool(), "audited-token").await;
    let (_, other) = seed(harness.pool(), "another-token").await;

    let (status, _) = send(
        &app,
        push(
            source,
            Some("audited-token"),
            Some("delivery-1"),
            r#"{"url":"https://example.test/a"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The same credential, at the other source's URL.
    let (status, _) = send(
        &app,
        push(
            other,
            Some("audited-token"),
            Some("delivery-2"),
            r#"{"url":"https://example.test/b"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A credential nobody holds.
    let (status, _) = send(
        &app,
        push(
            source,
            Some("not-a-credential"),
            Some("delivery-3"),
            r#"{"url":"https://example.test/c"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let rows: Vec<(String, String, Uuid)> = sqlx::query_as(
        "select action, outcome, actor_user_id from identity.audit_events order by occurred_at",
    )
    .fetch_all(harness.pool())
    .await
    .expect("the audit trail must read");

    assert_eq!(
        rows.iter()
            .map(|(action, outcome, _)| (action.as_str(), outcome.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("ingest.webhook.receive", "allowed"),
            ("ingest.webhook.receive", "denied"),
        ],
        "an accepted signal and the attributable refusal, and nothing for the anonymous attempt",
    );
    for (_, _, actor) in &rows {
        assert_eq!(
            *actor, owner,
            "every row is attributed to the credential's owner"
        );
    }

    harness.cleanup().await.expect("cleanup");
}

/// I-10. The allowance is per SOURCE, not per owner.
///
/// Two sources of one user are two independent senders. A provider that starts retrying in a loop
/// spends its own allowance and must not silence the other one — which a limiter keyed by owner
/// would let it do, and which is the more likely failure here than an attack: a webhook sender with
/// a bad retry policy is ordinary.
#[tokio::test]
async fn one_source_spending_its_allowance_does_not_silence_another() {
    let harness = TestDatabase::create().await.expect("a test database");
    let app = app_with(&harness, 1);
    let (_, noisy) = seed(harness.pool(), "noisy-token").await;
    let (_, quiet) = seed(harness.pool(), "quiet-token").await;

    let (status, _) = send(
        &app,
        push(
            noisy,
            Some("noisy-token"),
            Some("a"),
            r#"{"url":"https://example.test/a"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, body) = send(
        &app,
        push(
            noisy,
            Some("noisy-token"),
            Some("b"),
            r#"{"url":"https://example.test/b"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "platform.limit.rate_exceeded");

    let (status, _) = send(
        &app,
        push(
            quiet,
            Some("quiet-token"),
            Some("c"),
            r#"{"url":"https://example.test/c"}"#,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "the other source has its own allowance"
    );

    harness.cleanup().await.expect("cleanup");
}
