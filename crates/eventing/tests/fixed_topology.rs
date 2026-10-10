//! The fixed consumers and the KV bucket Edge provisions for the whole fleet (XR-021 CONTRACTS.md
//! section S04).
//!
//! The expected table is written out here, independently of the constants in `stream.rs`, so a
//! renamed durable or a changed filter fails this test instead of silently re-defining the contract
//! the other services hard-code.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use async_nats::jetstream;
use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, ReplayPolicy};
use platform_eventing::{COMMAND_STREAM, EventingError, StreamSpec, ensure_domain_topology};
use uuid::Uuid;

/// One row of the S04 tables.
struct Expected {
    stream: &'static str,
    durable: &'static str,
    filter: &'static str,
    ack: AckPolicy,
    deliver: DeliverPolicy,
    ack_wait_seconds: u64,
    max_deliver: i64,
}

const fn command(
    durable: &'static str,
    filter: &'static str,
    ack_wait_seconds: u64,
    max_deliver: i64,
) -> Expected {
    Expected {
        stream: "ratatoskr_commands",
        durable,
        filter,
        ack: AckPolicy::Explicit,
        deliver: DeliverPolicy::All,
        ack_wait_seconds,
        max_deliver,
    }
}

const fn event(durable: &'static str, filter: &'static str, ack_wait_seconds: u64) -> Expected {
    Expected {
        stream: "ratatoskr_events",
        durable,
        filter,
        ack: AckPolicy::Explicit,
        deliver: DeliverPolicy::All,
        ack_wait_seconds,
        max_deliver: -1,
    }
}

/// S04, command stream then event stream.
fn table() -> Vec<Expected> {
    vec![
        command(
            "ratatoskr_extractor_capture",
            "cmd.content.capture.requested.v1",
            30,
            12,
        ),
        command(
            "ratatoskr_browser_worker",
            "cmd.content.render.requested.v1",
            300,
            12,
        ),
        command(
            "ratatoskr_knowledge_channel_recap",
            "cmd.knowledge.channel_digest_recap.requested.v1",
            30,
            -1,
        ),
        command(
            "ratatoskr_channel_digest_subscriptions",
            "cmd.channel_digest.subscription.set_requested.v1",
            30,
            -1,
        ),
        command(
            "ratatoskr_channel_digest_runs",
            "cmd.channel_digest.run.requested.v1",
            30,
            -1,
        ),
        command(
            "ratatoskr_channel_digest_schedule_occurrences",
            "cmd.channel_digest.schedule.occurrence_requested.v1",
            30,
            -1,
        ),
        command(
            "ratatoskr_vault_backup_policy",
            "cmd.vault.backup_policy.apply_requested.v1",
            30,
            -1,
        ),
        event(
            "ratatoskr_knowledge_documents",
            "evt.content.document.extracted.v1",
            120,
        ),
        event(
            "ratatoskr_knowledge_social_sources",
            "evt.social.source.>",
            120,
        ),
        event("ratatoskr_knowledge_ai_archive", "evt.ai_archive.>", 120),
        event(
            "ratatoskr_knowledge_repository_requests",
            "evt.knowledge.repository_analysis.requested.v1",
            120,
        ),
        event(
            "ratatoskr_github_analysis_completed",
            "evt.knowledge.repository_analysis.completed.v1",
            30,
        ),
        event(
            "ratatoskr_github_analysis_failed",
            "evt.knowledge.repository_analysis.failed.v1",
            30,
        ),
        event(
            "ratatoskr_github_policy_acknowledged",
            "evt.vault.backup_policy.acknowledged.v1",
            30,
        ),
        event(
            "ratatoskr_x_extractor_reports",
            "evt.platform.operation.reported.v1",
            30,
        ),
        event(
            "ratatoskr_channel_digest_recap_completed",
            "evt.knowledge.channel_digest_recap.completed.v1",
            30,
        ),
        event(
            "ratatoskr_channel_digest_recap_failed",
            "evt.knowledge.channel_digest_recap.failed.v1",
            30,
        ),
        Expected {
            stream: "ratatoskr_events",
            durable: "ratatoskr_extractor_render_awaits",
            filter: "evt.content.render.>",
            ack: AckPolicy::None,
            deliver: DeliverPolicy::New,
            ack_wait_seconds: 30,
            max_deliver: -1,
        },
    ]
}

/// The broker the suite talks to. Matches `compose.yaml`.
#[expect(
    clippy::disallowed_methods,
    reason = "a test binary choosing which broker to talk to"
)]
fn nats_url() -> String {
    std::env::var("PLATFORM_TEST_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_owned())
}

async fn declare_streams(context: &jetstream::Context) {
    for spec in [StreamSpec::command_stream(), StreamSpec::event_stream()] {
        context
            .get_or_create_stream(spec.config())
            .await
            .expect("the stream is declared");
    }
}

#[tokio::test]
async fn ensure_domain_topology_creates_every_fixed_consumer_and_the_kv_bucket() {
    let client = async_nats::connect(nats_url())
        .await
        .expect("the test broker must be running (PLATFORM_TEST_NATS_URL)");
    let context = jetstream::new(client);
    declare_streams(&context).await;

    ensure_domain_topology(&context)
        .await
        .expect("the first call provisions the topology");

    for row in table() {
        let stream = context
            .get_stream(row.stream)
            .await
            .expect("the stream exists");
        let consumer: jetstream::consumer::PullConsumer = stream
            .get_consumer(row.durable)
            .await
            .unwrap_or_else(|error| panic!("{} is not provisioned: {error}", row.durable));
        let config = &consumer.cached_info().config;
        let name = row.durable;
        assert_eq!(config.durable_name.as_deref(), Some(name), "{name}");
        assert_eq!(config.filter_subject, row.filter, "{name} filter");
        assert_eq!(config.ack_policy, row.ack, "{name} ack policy");
        assert_eq!(config.deliver_policy, row.deliver, "{name} deliver policy");
        assert_eq!(
            config.ack_wait,
            Duration::from_secs(row.ack_wait_seconds),
            "{name} ack wait"
        );
        assert_eq!(config.max_deliver, row.max_deliver, "{name} max_deliver");
        assert_eq!(config.replay_policy, ReplayPolicy::Instant, "{name} replay");
        assert!(
            config.deliver_subject.is_none(),
            "{name} must be a pull consumer"
        );
    }

    let mut bucket = context
        .get_stream("KV_browser_worker_completions")
        .await
        .expect("the browser-worker completion bucket exists");
    let info = bucket.info().await.expect("the bucket describes itself");
    assert_eq!(info.config.max_age, Duration::from_hours(24));
    assert!(
        info.config.allow_direct,
        "the worker reads it with direct get"
    );

    ensure_domain_topology(&context)
        .await
        .expect("a second call changes nothing and succeeds");
}

/// A disposable broker, so that a deliberately conflicting durable never lands on the shared one.
struct Broker {
    container: String,
    directory: PathBuf,
    url: String,
}

impl Broker {
    fn start() -> Self {
        let container = format!("ratatoskr-platform-topology-{}", Uuid::now_v7().simple());
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/nats-topology-fixtures")
            .join(&container);
        std::fs::create_dir_all(&directory).expect("the scratch directory");
        let started = Command::new("docker")
            .args([
                "run",
                "--detach",
                "--name",
                &container,
                "--publish",
                "127.0.0.1::4222",
                "nats:2-alpine",
                "-js",
            ])
            .output()
            .expect("docker must start the disposable broker");
        assert!(
            started.status.success(),
            "the disposable broker did not start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let port = Command::new("docker")
            .args(["port", &container, "4222/tcp"])
            .output()
            .expect("docker reports the published port");
        let binding = String::from_utf8(port.stdout).expect("UTF-8");
        let port = binding
            .trim()
            .lines()
            .next()
            .and_then(|line| line.rsplit_once(':'))
            .map(|(_, port)| port.to_owned())
            .expect("a published port");
        Self {
            container,
            directory,
            url: format!("nats://127.0.0.1:{port}"),
        }
    }

    async fn connect(&self) -> async_nats::Client {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match async_nats::connect(&self.url).await {
                Ok(client) => return client,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("the disposable broker did not accept a connection: {error}"),
            }
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.container])
            .output();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn a_durable_that_differs_from_its_spec_refuses_the_topology() {
    let broker = Broker::start();
    let context = jetstream::new(broker.connect().await);
    declare_streams(&context).await;
    let stream = context
        .get_stream(COMMAND_STREAM)
        .await
        .expect("the command stream");
    stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: Some("ratatoskr_extractor_capture".to_owned()),
            filter_subject: "cmd.content.render.requested.v1".to_owned(),
            ack_policy: AckPolicy::Explicit,
            ..jetstream::consumer::pull::Config::default()
        })
        .await
        .expect("the conflicting durable is created");

    let refused = ensure_domain_topology(&context).await;
    assert!(
        matches!(refused, Err(EventingError::Bus(_))),
        "a durable with another filter must be refused, got {refused:?}"
    );
}
