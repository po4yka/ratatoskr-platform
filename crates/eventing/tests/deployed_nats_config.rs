//! The deployed `deploy/nats/ratatoskr.conf`, proven on a real broker (XR-021 CONTRACTS.md S03).
//!
//! The file as committed carries `UREPLACE_ME_*` placeholders, which `nats-server` refuses. Every
//! test here renders it with `platform-nats-profile` (the same procedure `deploy/nats/README.md`
//! gives an operator), starts `nats:2-alpine` on the rendered copy exactly as `compose.yaml` does,
//! and connects through the published port as one identity. What the identity may do is observed on
//! the wire: an allowed request is answered or acknowledged, a refused one never is (the client
//! cannot tell a permission denial from silence; the server log says `Publish Violation`).
//!
//! The matrix is one test per identity. Each starts its own broker, lets EDGE provision the topology,
//! feeds the durables the identity owns through the identity that is allowed to publish to them, and
//! then exercises every grant and every refusal in the contract.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::collections::BTreeMap;
use std::time::Duration;

use async_nats::jetstream;
use futures_util::StreamExt as _;
use futures_util::future::join_all;
use platform_eventing::{StreamSpec, ensure_fixed_topology};
use tokio::time::timeout;

mod support;

use support::{Container, REQUEST_TIMEOUT, connect};

const CONF: &str = include_str!("../../../deploy/nats/ratatoskr.conf");

const CMD: &str = "ratatoskr_commands";
const EVT: &str = "ratatoskr_events";

/// Every durable, the identity that owns it and the stream it lives on. EDGE owns the three that
/// Platform consumes itself, listed separately because no other identity may even describe them.
const OWNERS: [(&str, &str, &str); 24] = [
    ("TELEGRAM", EVT, "ratatoskr_telegram_notifications"),
    ("X", CMD, "ratatoskr_x_browser_capture"),
    ("X", EVT, "ratatoskr_x_extractor_reports"),
    ("INSTAGRAM", CMD, "ratatoskr_instagram_browser_capture"),
    ("THREADS", CMD, "threads_browser_capture"),
    ("EXTRACTOR", CMD, "ratatoskr_extractor_capture"),
    ("EXTRACTOR", EVT, "ratatoskr_extractor_render_awaits"),
    ("EXTRACTOR_BROWSER_WORKER", CMD, "ratatoskr_browser_worker"),
    ("KNOWLEDGE", CMD, "ratatoskr_knowledge_channel_recap"),
    ("KNOWLEDGE", EVT, "ratatoskr_knowledge_documents"),
    ("KNOWLEDGE", EVT, "ratatoskr_knowledge_social_sources"),
    ("KNOWLEDGE", EVT, "ratatoskr_knowledge_ai_archive"),
    ("KNOWLEDGE", EVT, "ratatoskr_knowledge_repository_requests"),
    ("GITHUB", EVT, "ratatoskr_github_analysis_completed"),
    ("GITHUB", EVT, "ratatoskr_github_analysis_failed"),
    ("GITHUB", EVT, "ratatoskr_github_policy_acknowledged"),
    ("VAULT", CMD, "ratatoskr_vault_backup_policy"),
    (
        "CHANNEL_DIGESTS",
        CMD,
        "ratatoskr_channel_digest_subscriptions",
    ),
    ("CHANNEL_DIGESTS", CMD, "ratatoskr_channel_digest_runs"),
    (
        "CHANNEL_DIGESTS",
        CMD,
        "ratatoskr_channel_digest_schedule_occurrences",
    ),
    (
        "CHANNEL_DIGESTS",
        EVT,
        "ratatoskr_channel_digest_recap_completed",
    ),
    (
        "CHANNEL_DIGESTS",
        EVT,
        "ratatoskr_channel_digest_recap_failed",
    ),
    ("EDGE", EVT, "platform_ai_archive_chatgpt_projection"),
    ("EDGE", EVT, "platform_ai_archive_claude_projection"),
];

/// Durables only Edge may describe. `platform_edge_projection` is created by Edge's own consumer,
/// so it may not exist on this broker, which does not change who is refused.
const EDGE_ONLY: [(&str, &str); 1] = [(EVT, "platform_edge_projection")];

/// Where each durable's test message comes from: the identity allowed to publish to its filter, and
/// one concrete subject that matches it.
fn source_of(durable: &str) -> (&'static str, &'static str) {
    match durable {
        "ratatoskr_x_browser_capture" => ("EDGE", "cmd.x.capture.requested.v1"),
        "ratatoskr_instagram_browser_capture" => ("EDGE", "cmd.instagram.capture.requested.v1"),
        "threads_browser_capture" => ("EDGE", "cmd.threads.capture.requested.v1"),
        "ratatoskr_extractor_capture" => ("EDGE", "cmd.content.capture.requested.v1"),
        "ratatoskr_browser_worker" => ("EDGE", "cmd.content.render.requested.v1"),
        "ratatoskr_knowledge_channel_recap" => {
            ("EDGE", "cmd.knowledge.channel_digest_recap.requested.v1")
        }
        "ratatoskr_vault_backup_policy" => ("EDGE", "cmd.vault.backup_policy.apply_requested.v1"),
        "ratatoskr_channel_digest_subscriptions" => {
            ("EDGE", "cmd.channel_digest.subscription.set_requested.v1")
        }
        "ratatoskr_channel_digest_runs" => ("EDGE", "cmd.channel_digest.run.requested.v1"),
        "ratatoskr_channel_digest_schedule_occurrences" => (
            "EDGE",
            "cmd.channel_digest.schedule.occurrence_requested.v1",
        ),
        "ratatoskr_knowledge_documents" => ("EXTRACTOR", "evt.content.document.extracted.v1"),
        "ratatoskr_x_extractor_reports" => ("EXTRACTOR", "evt.platform.operation.reported.v1"),
        "ratatoskr_extractor_render_awaits" => (
            "EXTRACTOR_BROWSER_WORKER",
            "evt.content.render.completed.v1",
        ),
        "ratatoskr_knowledge_social_sources" => ("X", "evt.social.source.captured.v1"),
        "ratatoskr_knowledge_ai_archive" => ("CHATGPT", "evt.ai_archive.archive.imported.v1"),
        "ratatoskr_knowledge_repository_requests" => {
            ("GITHUB", "evt.knowledge.repository_analysis.requested.v1")
        }
        "ratatoskr_github_analysis_completed" => (
            "KNOWLEDGE",
            "evt.knowledge.repository_analysis.completed.v1",
        ),
        "ratatoskr_github_analysis_failed" => {
            ("KNOWLEDGE", "evt.knowledge.repository_analysis.failed.v1")
        }
        "ratatoskr_github_policy_acknowledged" => {
            ("VAULT", "evt.vault.backup_policy.acknowledged.v1")
        }
        "ratatoskr_channel_digest_recap_completed" => (
            "KNOWLEDGE",
            "evt.knowledge.channel_digest_recap.completed.v1",
        ),
        "ratatoskr_channel_digest_recap_failed" => {
            ("KNOWLEDGE", "evt.knowledge.channel_digest_recap.failed.v1")
        }
        other => panic!("no message source is defined for {other}"),
    }
}

/// The deployed configuration, rendered and running.
struct Deployed {
    container: Container,
    seeds: BTreeMap<String, String>,
}

impl Deployed {
    fn start() -> Self {
        let rendered = platform_nats_profile::render(CONF).expect("the deployed ACL renders");
        let seeds = rendered
            .seeds
            .iter()
            .map(|seed| (seed.name.clone(), seed.seed.clone()))
            .collect();
        // `--user root`: the container writes its JetStream store under the configured NVMe path,
        // which the unprivileged image user cannot create. The host deployment gives the same path
        // to a dedicated uid instead (`compose.yaml`).
        let container = Container::start("deployed-acl", &rendered.conf, &["--user", "root"]);
        Self { container, seeds }
    }

    async fn client(&self, identity: &str) -> async_nats::Client {
        let seed = self
            .seeds
            .get(identity)
            .unwrap_or_else(|| panic!("the deployed ACL has no {identity} identity"));
        connect(&self.container.url, seed).await
    }

    /// EDGE declares both streams and every fixed durable, as `ratatoskr-edge` does at startup.
    async fn provision(&self) {
        let context = jetstream::new(self.client("EDGE").await);
        for spec in [StreamSpec::command_stream(), StreamSpec::event_stream()] {
            context
                .get_or_create_stream(spec.config())
                .await
                .expect("EDGE creates the streams");
        }
        ensure_fixed_topology(&context)
            .await
            .expect("EDGE provisions every fixed durable and the KV bucket");
    }
}

/// Publish and wait for the stream's acknowledgement. `Err` is the observable form of a refusal.
async fn publish_acked(client: &async_nats::Client, subject: &str) -> Result<(), String> {
    let context = jetstream::new(client.clone());
    let pending = context
        .publish(subject.to_owned(), "{}".into())
        .await
        .map_err(|error| error.to_string())?;
    match timeout(REQUEST_TIMEOUT + Duration::from_millis(500), pending).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("no acknowledgement".to_owned()),
    }
}

/// Fetch one message from a durable and acknowledge it with the server's confirmation.
async fn fetch_and_ack(
    client: &async_nats::Client,
    stream: &str,
    durable: &str,
) -> Result<(), String> {
    let consumer: jetstream::consumer::PullConsumer = jetstream::new(client.clone())
        .get_consumer_from_stream(durable, stream)
        .await
        .map_err(|error| format!("describing {durable}: {error}"))?;
    let mut batch = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .map_err(|error| format!("fetching from {durable}: {error}"))?;
    let message = batch
        .next()
        .await
        .ok_or_else(|| format!("{durable} delivered nothing"))?
        .map_err(|error| format!("{durable} delivery: {error}"))?;
    message
        .double_ack()
        .await
        .map_err(|error| format!("acknowledging on {durable}: {error}"))
}

/// Fetch from a durable that takes no acknowledgements.
async fn fetch_unacked(
    client: &async_nats::Client,
    stream: &str,
    durable: &str,
) -> Result<(), String> {
    let consumer: jetstream::consumer::PullConsumer = jetstream::new(client.clone())
        .get_consumer_from_stream(durable, stream)
        .await
        .map_err(|error| format!("describing {durable}: {error}"))?;
    let mut batch = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .map_err(|error| format!("fetching from {durable}: {error}"))?;
    batch
        .next()
        .await
        .ok_or_else(|| format!("{durable} delivered nothing"))?
        .map(|_| ())
        .map_err(|error| format!("{durable} delivery: {error}"))
}

/// Whether a raw `JetStream` API request is answered at all. Any answer, even an error body, means the
/// server accepted the publish; silence means it was refused.
async fn answered(client: &async_nats::Client, subject: &str, body: &str) -> bool {
    matches!(
        timeout(
            REQUEST_TIMEOUT + Duration::from_millis(500),
            client.request(subject.to_owned(), body.to_owned().into()),
        )
        .await,
        Ok(Ok(_))
    )
}

/// The requests that create a consumer, in every spelling the API has, on both streams.
fn consumer_creations() -> Vec<(String, String)> {
    let mut requests = Vec::new();
    for stream in [CMD, EVT] {
        let body = format!(
            r#"{{"stream_name":"{stream}","config":{{"durable_name":"stolen","filter_subject":">","ack_policy":"explicit"}}}}"#
        );
        for subject in [
            format!("$JS.API.CONSUMER.CREATE.{stream}"),
            format!("$JS.API.CONSUMER.CREATE.{stream}.stolen"),
            format!("$JS.API.CONSUMER.DURABLE.CREATE.{stream}.stolen"),
        ] {
            requests.push((subject, body.clone()));
        }
    }
    requests
}

/// Everything every non-EDGE identity is refused.
async fn assert_no_broad_access(client: &async_nats::Client, identity: &str) {
    let mut requests = consumer_creations();
    for subject in [
        format!("$JS.API.STREAM.MSG.GET.{CMD}"),
        format!("$JS.API.DIRECT.GET.{CMD}"),
        "$JS.API.STREAM.CREATE.stolen".to_owned(),
    ] {
        requests.push((subject, r#"{"seq":1}"#.to_owned()));
    }
    let refusals =
        join_all(requests.into_iter().map(|(subject, body)| async move {
            (answered(client, &subject, &body).await, subject)
        }))
        .await;
    for (was_answered, subject) in refusals {
        assert!(!was_answered, "{identity} reached {subject}");
    }
}

/// Every non-EDGE identity is refused `CONSUMER.INFO` on every durable it does not own.
async fn assert_only_own_durables(client: &async_nats::Client, identity: &str) {
    let probes = OWNERS
        .iter()
        .map(|(owner, stream, durable)| (*owner, *stream, *durable))
        .chain(
            EDGE_ONLY
                .iter()
                .map(|(stream, durable)| ("EDGE", *stream, *durable)),
        );
    let results = join_all(probes.map(|(owner, stream, durable)| async move {
        let subject = format!("$JS.API.CONSUMER.INFO.{stream}.{durable}");
        (owner, durable, answered(client, &subject, "").await)
    }))
    .await;
    for (owner, durable, was_answered) in results {
        if owner == identity {
            assert!(was_answered, "{identity} cannot describe its own {durable}");
        } else {
            assert!(!was_answered, "{identity} can describe {owner}'s {durable}");
        }
    }
}

/// Feed every durable `identity` owns, through the identity allowed to publish to it.
async fn feed(broker: &Deployed, identity: &str) {
    for (owner, _, durable) in OWNERS {
        if owner != identity || durable == "ratatoskr_telegram_notifications" {
            continue;
        }
        let (publisher, subject) = source_of(durable);
        let client = broker.client(publisher).await;
        publish_acked(&client, subject)
            .await
            .unwrap_or_else(|error| panic!("{publisher} could not publish {subject}: {error}"));
    }
}

/// What one identity is granted, exercised end to end, then what it is refused.
async fn exercise(identity: &str, publishes: &[&str]) {
    let broker = Deployed::start();
    broker.provision().await;
    feed(&broker, identity).await;
    let client = broker.client(identity).await;

    for (owner, stream, durable) in OWNERS {
        if owner != identity || durable == "ratatoskr_telegram_notifications" {
            continue;
        }
        let fetched = if durable == "ratatoskr_extractor_render_awaits" {
            fetch_unacked(&client, stream, durable).await
        } else {
            fetch_and_ack(&client, stream, durable).await
        };
        fetched.unwrap_or_else(|error| panic!("{identity} was refused its own durable: {error}"));
    }
    for subject in publishes {
        publish_acked(&client, subject)
            .await
            .unwrap_or_else(|error| panic!("{identity} was refused {subject}: {error}"));
    }

    assert_only_own_durables(&client, identity).await;
    assert_no_broad_access(&client, identity).await;
    let notification = publish_acked(&client, "evt.platform.notification.raised.v1").await;
    assert!(
        notification.is_err(),
        "{identity} published a notification it has no authority to raise"
    );
}

#[tokio::test]
async fn the_deployed_config_is_reachable_through_a_published_port() {
    let broker = Deployed::start();
    let edge = broker.client("EDGE").await;
    let context = jetstream::new(edge);
    timeout(Duration::from_secs(10), context.query_account())
        .await
        .expect("the account request is answered within 10 seconds")
        .expect("EDGE can read its JetStream account through the published port");
}

#[tokio::test]
async fn edge_identity_provisions_and_publishes_commands_but_never_events() {
    let broker = Deployed::start();
    broker.provision().await;
    let edge = broker.client("EDGE").await;
    publish_acked(&edge, "cmd.content.capture.requested.v1")
        .await
        .expect("EDGE publishes a command with an acknowledgement");
    for refused in [
        "evt.platform.notification.raised.v1",
        "evt.content.document.extracted.v1",
    ] {
        assert!(
            publish_acked(&edge, refused).await.is_err(),
            "EDGE published the event {refused}"
        );
    }
    for destructive in [
        format!("$JS.API.STREAM.DELETE.{CMD}"),
        format!("$JS.API.STREAM.PURGE.{EVT}"),
    ] {
        assert!(
            !answered(&edge, &destructive, "").await,
            "EDGE reached {destructive}, which its deny list forbids"
        );
    }
}

#[tokio::test]
async fn extractor_identity_matrix() {
    exercise(
        "EXTRACTOR",
        &[
            "evt.content.document.extracted.v1",
            "evt.platform.operation.reported.v1",
            "cmd.content.render.requested.v1",
        ],
    )
    .await;
    let broker = Deployed::start();
    broker.provision().await;
    let extractor = broker.client("EXTRACTOR").await;
    for refused in [
        "evt.social.source.captured.v1",
        "evt.ai-archive.chatgpt.operation.reported.v1",
        "evt.content.render.completed.v1",
        "cmd.content.capture.requested.v1",
    ] {
        assert!(
            publish_acked(&extractor, refused).await.is_err(),
            "EXTRACTOR published {refused}"
        );
    }
}

#[tokio::test]
async fn extractor_browser_worker_identity_matrix() {
    exercise(
        "EXTRACTOR_BROWSER_WORKER",
        &[
            "evt.content.render.completed.v1",
            "evt.content.render.failed.v1",
        ],
    )
    .await;

    let broker = Deployed::start();
    broker.provision().await;
    let worker = broker.client("EXTRACTOR_BROWSER_WORKER").await;
    let context = jetstream::new(worker.clone());
    let bucket = context
        .get_key_value("browser_worker_completions")
        .await
        .expect("the worker opens the bucket Edge provisioned");
    assert!(
        bucket
            .get("render-1")
            .await
            .expect("an absent key is a read, not an error")
            .is_none()
    );
    bucket
        .put("render-1", "done".into())
        .await
        .expect("the worker writes a completion marker");
    let stored = bucket
        .get("render-1")
        .await
        .expect("the worker reads the marker back")
        .expect("the marker exists");
    assert_eq!(&stored[..], b"done");

    for refused in [
        "evt.platform.operation.reported.v1",
        "cmd.content.render.requested.v1",
    ] {
        assert!(
            publish_acked(&worker, refused).await.is_err(),
            "the browser worker published {refused}"
        );
    }
    let creation = context
        .create_stream(jetstream::stream::Config {
            name: "stolen".to_owned(),
            subjects: vec!["stolen.>".to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await;
    assert!(creation.is_err(), "the browser worker created a stream");
}

#[tokio::test]
async fn knowledge_identity_matrix() {
    exercise(
        "KNOWLEDGE",
        &[
            "evt.knowledge.analysis.completed.v1",
            "evt.knowledge.ai_archive_analysis.completed.v1",
            "evt.knowledge.repository_analysis.completed.v1",
            "evt.knowledge.repository_analysis.failed.v1",
            "evt.knowledge.channel_digest_recap.completed.v1",
            "evt.knowledge.channel_digest_recap.failed.v1",
        ],
    )
    .await;
}

#[tokio::test]
async fn github_identity_matrix() {
    exercise(
        "GITHUB",
        &[
            "evt.knowledge.repository_analysis.requested.v1",
            "cmd.vault.backup_policy.apply_requested.v1",
        ],
    )
    .await;
}

#[tokio::test]
async fn vault_identity_matrix() {
    exercise("VAULT", &["evt.vault.backup_policy.acknowledged.v1"]).await;
}

#[tokio::test]
async fn channel_digests_identity_matrix() {
    exercise(
        "CHANNEL_DIGESTS",
        &[
            "cmd.knowledge.channel_digest_recap.requested.v1",
            "evt.platform.operation.reported.v1",
            "cmd.platform.schedule.registration_requested.v1",
        ],
    )
    .await;
}

#[tokio::test]
async fn x_identity_matrix() {
    exercise(
        "X",
        &[
            "evt.platform.operation.reported.v1",
            "evt.social.source.captured.v1",
            "evt.social.source.updated.v1",
            "evt.social.source.removed.v1",
            "cmd.content.capture.requested.v1",
        ],
    )
    .await;
}

#[tokio::test]
async fn instagram_identity_matrix() {
    exercise(
        "INSTAGRAM",
        &[
            "evt.platform.operation.reported.v1",
            "evt.social.source.captured.v1",
            "evt.social.source.updated.v1",
            "evt.social.source.removed.v1",
        ],
    )
    .await;
}

#[tokio::test]
async fn threads_identity_matrix() {
    exercise(
        "THREADS",
        &[
            "evt.platform.operation.reported.v1",
            "evt.social.source.captured.v1",
            "evt.social.source.updated.v1",
            "evt.social.source.removed.v1",
        ],
    )
    .await;
}

const AI_ARCHIVE_FACTS: [&str; 8] = [
    "evt.ai_archive.archive.imported.v1",
    "evt.ai_archive.conversation.added.v1",
    "evt.ai_archive.conversation.updated.v1",
    "evt.ai_archive.project.added.v1",
    "evt.ai_archive.project.updated.v1",
    "evt.ai_archive.artifact.added.v1",
    "evt.ai_archive.artifact.updated.v1",
    "evt.ai_archive.subject.tombstoned.v1",
];

#[tokio::test]
async fn chatgpt_identity_matrix() {
    let mut granted = vec!["evt.ai-archive.chatgpt.operation.reported.v1"];
    granted.extend(AI_ARCHIVE_FACTS);
    exercise("CHATGPT", &granted).await;

    let broker = Deployed::start();
    broker.provision().await;
    let chatgpt = broker.client("CHATGPT").await;
    assert!(
        publish_acked(&chatgpt, "evt.ai-archive.claude.operation.reported.v1")
            .await
            .is_err(),
        "ChatGPT impersonated Claude"
    );
}

#[tokio::test]
async fn claude_identity_matrix() {
    let mut granted = vec!["evt.ai-archive.claude.operation.reported.v1"];
    granted.extend(AI_ARCHIVE_FACTS);
    exercise("CLAUDE", &granted).await;

    let broker = Deployed::start();
    broker.provision().await;
    let claude = broker.client("CLAUDE").await;
    assert!(
        publish_acked(&claude, "evt.ai-archive.chatgpt.operation.reported.v1")
            .await
            .is_err(),
        "Claude impersonated ChatGPT"
    );
}

#[tokio::test]
async fn telegram_identity_may_only_describe_its_own_durable() {
    let broker = Deployed::start();
    broker.provision().await;
    let telegram = broker.client("TELEGRAM").await;
    assert_only_own_durables(&telegram, "TELEGRAM").await;
    assert_no_broad_access(&telegram, "TELEGRAM").await;
}
