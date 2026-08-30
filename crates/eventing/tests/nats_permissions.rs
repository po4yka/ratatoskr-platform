//! Real-broker proof for least-privilege application identities.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use async_nats::jetstream;
use futures_util::StreamExt as _;
use nkeys::KeyPair;
use platform_eventing::{
    COMMAND_STREAM, EVENT_STREAM, GITHUB_CONSUMERS, TELEGRAM_NOTIFICATION_CONSUMER,
    TELEGRAM_NOTIFICATION_SUBJECT,
};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct NatsFixture {
    container: String,
    directory: PathBuf,
    url: String,
    admin_seed: String,
    github_seed: String,
    telegram_seed: String,
}

impl NatsFixture {
    fn start(include_telegram_identity: bool) -> Self {
        let suffix = Uuid::now_v7().simple().to_string();
        let container = format!("ratatoskr-platform-nats-permissions-{suffix}");
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/nats-permission-fixtures")
            .join(&container);
        std::fs::create_dir_all(&directory).expect("the disposable NATS directory");

        let admin = KeyPair::new_user();
        let github = KeyPair::new_user();
        let telegram = KeyPair::new_user();
        let telegram_public = include_telegram_identity.then(|| telegram.public_key());
        let config = nats_config(
            &admin.public_key(),
            telegram_public.as_deref(),
            Some(&github.public_key()),
        );
        let config_path = directory.join("nats.conf");
        std::fs::write(&config_path, config).expect("the disposable NATS configuration");

        let mount = format!("{}:/etc/nats-fixture:ro", directory.display());
        let started = Command::new("docker")
            .args([
                "run",
                "--detach",
                "--name",
                &container,
                "--publish",
                "127.0.0.1::4222",
                "--volume",
                &mount,
                "nats:2-alpine",
                "-c",
                "/etc/nats-fixture/nats.conf",
            ])
            .output()
            .expect("docker must start the disposable NATS server");
        assert!(
            started.status.success(),
            "disposable NATS failed to start: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        let port = Command::new("docker")
            .args(["port", &container, "4222/tcp"])
            .output()
            .expect("docker must report the disposable NATS port");
        if !port.status.success() {
            let logs = Command::new("docker")
                .args(["logs", &container])
                .output()
                .expect("docker must report why disposable NATS exited");
            let _ = Command::new("docker")
                .args(["rm", "--force", &container])
                .output();
            let _ = std::fs::remove_dir_all(&directory);
            panic!(
                "docker did not report the NATS port: {}{}",
                String::from_utf8_lossy(&port.stderr),
                String::from_utf8_lossy(&logs.stderr)
            );
        }
        let binding = String::from_utf8(port.stdout).expect("the port binding is UTF-8");
        let port = binding
            .trim()
            .rsplit_once(':')
            .map(|(_, port)| port)
            .expect("the port binding has a port");

        Self {
            container,
            directory,
            url: format!("nats://127.0.0.1:{port}"),
            admin_seed: admin.seed().expect("the disposable admin seed"),
            github_seed: github.seed().expect("the disposable GitHub seed"),
            telegram_seed: telegram.seed().expect("the disposable Telegram seed"),
        }
    }

    async fn connect(&self, seed: &str) -> async_nats::Client {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match async_nats::ConnectOptions::with_nkey(seed.to_owned())
                .request_timeout(Some(REQUEST_TIMEOUT))
                .connect(&self.url)
                .await
            {
                Ok(client) => return client,
                Err(error) if tokio::time::Instant::now() < deadline => {
                    let _ = error;
                    sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("the disposable NATS identity did not connect: {error}"),
            }
        }
    }
}

fn nats_config(
    admin_public: &str,
    telegram_public: Option<&str>,
    github_public: Option<&str>,
) -> String {
    let stream = EVENT_STREAM;
    let consumer = TELEGRAM_NOTIFICATION_CONSUMER;
    let telegram_user = telegram_public.map_or_else(String::new, |public_key| {
        format!(
            r#"
        {{
            nkey: {public_key}
            permissions: {{
                publish: {{
                    allow: [
                        "$JS.API.CONSUMER.INFO.{stream}.{consumer}",
                        "$JS.API.CONSUMER.MSG.NEXT.{stream}.{consumer}",
                        "$JS.ACK.{stream}.{consumer}.>",
                    ]
                }}
                subscribe: {{ allow: ["_INBOX.>"] }}
            }}
        }}"#
        )
    });
    let github_user = github_public.map_or_else(String::new, |public_key| {
        let permissions = GITHUB_CONSUMERS
            .iter()
            .flat_map(|spec| {
                [
                    format!(
                        "\"$JS.API.CONSUMER.INFO.{}.{}\"",
                        spec.stream_name, spec.durable_name
                    ),
                    format!(
                        "\"$JS.API.CONSUMER.MSG.NEXT.{}.{}\"",
                        spec.stream_name, spec.durable_name
                    ),
                    format!("\"$JS.ACK.{}.{}.>\"", spec.stream_name, spec.durable_name),
                ]
            })
            .chain([
                "\"evt.knowledge.repository_analysis.requested.v1\"".to_owned(),
                "\"cmd.vault.target.desired.v1\"".to_owned(),
            ])
            .collect::<Vec<_>>()
            .join(",\n                        ");
        format!(
            r#"
        {{
            nkey: {public_key}
            permissions: {{
                publish: {{ allow: [{permissions}] }}
                subscribe: {{ allow: ["_INBOX.>"] }}
            }}
        }}"#
        )
    });
    let telegram_separator = if telegram_public.is_some() { "," } else { "" };
    let github_separator = if github_public.is_some() { "," } else { "" };
    format!(
        r"
port: 4222
host: 0.0.0.0
jetstream {{ store_dir: /data }}
authorization {{
    users: [
        {{ nkey: {admin_public} }}{telegram_separator}
        {telegram_user}{github_separator}
        {github_user}
    ]
}}
"
    )
}

impl Drop for NatsFixture {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.container])
            .output();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn telegram_nkey_permission_matrix_is_enforced_by_nats() {
    let fixture = NatsFixture::start(true);
    let admin = fixture.connect(&fixture.admin_seed).await;
    let admin_jetstream = jetstream::new(admin);
    let stream = admin_jetstream
        .create_stream(jetstream::stream::Config {
            name: EVENT_STREAM.to_owned(),
            subjects: vec!["evt.>".to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the admin creates the event stream");
    stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: Some(TELEGRAM_NOTIFICATION_CONSUMER.to_owned()),
            filter_subject: TELEGRAM_NOTIFICATION_SUBJECT.to_owned(),
            ack_policy: jetstream::consumer::AckPolicy::Explicit,
            ..jetstream::consumer::pull::Config::default()
        })
        .await
        .expect("the admin creates the fixed Telegram durable");
    stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: Some("foreign_notifications".to_owned()),
            filter_subject: TELEGRAM_NOTIFICATION_SUBJECT.to_owned(),
            ack_policy: jetstream::consumer::AckPolicy::Explicit,
            ..jetstream::consumer::pull::Config::default()
        })
        .await
        .expect("the admin creates a foreign durable for the denial proof");
    admin_jetstream
        .publish(TELEGRAM_NOTIFICATION_SUBJECT, "notification".into())
        .await
        .expect("the admin can publish the fixture event")
        .await
        .expect("the fixture event is stored");

    let telegram = fixture.connect(&fixture.telegram_seed).await;
    let telegram_jetstream = jetstream::new(telegram.clone());
    let consumer: jetstream::consumer::PullConsumer = telegram_jetstream
        .get_consumer_from_stream(TELEGRAM_NOTIFICATION_CONSUMER, EVENT_STREAM)
        .await
        .expect("Telegram can describe only its fixed durable");
    let mut messages = consumer
        .messages()
        .await
        .expect("Telegram can fetch from its fixed durable");
    let message = timeout(REQUEST_TIMEOUT, messages.next())
        .await
        .expect("the allowed fetch returns promptly")
        .expect("the fixture event is delivered")
        .expect("the delivered event is valid");
    message
        .ack()
        .await
        .expect("Telegram can acknowledge its event");

    for forbidden in [
        format!("$JS.API.CONSUMER.DURABLE.CREATE.{EVENT_STREAM}.arbitrary"),
        format!("$JS.API.CONSUMER.MSG.NEXT.{EVENT_STREAM}.foreign_notifications"),
        TELEGRAM_NOTIFICATION_SUBJECT.to_owned(),
    ] {
        let response = timeout(
            REQUEST_TIMEOUT + Duration::from_millis(250),
            telegram.request(forbidden.clone(), "{}".into()),
        )
        .await;
        assert!(
            !matches!(response, Ok(Ok(_))),
            "Telegram unexpectedly received a response after publishing to {forbidden}"
        );
    }
}

#[tokio::test]
async fn github_identity_can_use_only_declared_bus_paths() {
    const OUTBOUND: [&str; 2] = [
        "evt.knowledge.repository_analysis.requested.v1",
        "cmd.vault.target.desired.v1",
    ];

    let fixture = NatsFixture::start(true);
    let admin = fixture.connect(&fixture.admin_seed).await;
    let admin_jetstream = jetstream::new(admin.clone());
    admin_jetstream
        .create_stream(jetstream::stream::Config {
            name: COMMAND_STREAM.to_owned(),
            subjects: vec!["cmd.>".to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the admin creates the command stream");
    admin_jetstream
        .create_stream(jetstream::stream::Config {
            name: EVENT_STREAM.to_owned(),
            subjects: vec!["evt.>".to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the admin creates the event stream");
    for spec in GITHUB_CONSUMERS {
        let stream = admin_jetstream
            .get_stream(spec.stream_name)
            .await
            .expect("the owning stream exists");
        stream
            .create_consumer(jetstream::consumer::pull::Config {
                durable_name: Some(spec.durable_name.to_owned()),
                filter_subject: spec.filter_subject.to_owned(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ack_wait: spec.ack_wait,
                max_deliver: spec.max_deliver,
                ..jetstream::consumer::pull::Config::default()
            })
            .await
            .expect("the admin creates the fixed GitHub durable");
        admin_jetstream
            .publish(spec.filter_subject, "inbound".into())
            .await
            .expect("the admin publishes the inbound fixture")
            .await
            .expect("the inbound fixture is stored");
    }

    let github = fixture.connect(&fixture.github_seed).await;
    let github_jetstream = jetstream::new(github.clone());
    for subject in OUTBOUND {
        github_jetstream
            .publish(subject, "outbound".into())
            .await
            .expect("GitHub may publish its declared family")
            .await
            .expect("the declared outbound message is stored");
    }
    for spec in GITHUB_CONSUMERS {
        let consumer: jetstream::consumer::PullConsumer = github_jetstream
            .get_consumer_from_stream(spec.durable_name, spec.stream_name)
            .await
            .expect("GitHub may inspect only its fixed durable");
        let mut messages = consumer.messages().await.expect("GitHub may fetch");
        let message = timeout(REQUEST_TIMEOUT, messages.next())
            .await
            .expect("the allowed fetch returns promptly")
            .expect("the fixture delivery exists")
            .expect("the fixture delivery is valid");
        message.ack().await.expect("GitHub may acknowledge");
    }

    assert_github_denials(&admin, &github, &github_jetstream).await;
}

async fn assert_github_denials(
    admin: &async_nats::Client,
    github: &async_nats::Client,
    github_jetstream: &jetstream::Context,
) {
    let foreign = "evt.social.source.captured.v1";
    let mut wildcard = github
        .subscribe("evt.>")
        .await
        .expect("the client creates a local subscription handle");
    github
        .flush()
        .await
        .expect("the server processes the subscribe");
    admin
        .publish(foreign, "foreign".into())
        .await
        .expect("the admin publishes a foreign event");
    assert!(
        timeout(REQUEST_TIMEOUT, wildcard.next()).await.is_err(),
        "GitHub must not receive a direct wildcard subscription"
    );

    let foreign_result: Result<jetstream::consumer::PullConsumer, _> = github_jetstream
        .get_consumer_from_stream("foreign", EVENT_STREAM)
        .await;
    assert!(
        foreign_result.is_err(),
        "GitHub must not inspect an unrelated durable"
    );
    for forbidden in [
        foreign.to_owned(),
        format!("$JS.API.CONSUMER.DURABLE.CREATE.{EVENT_STREAM}.arbitrary"),
        format!("$JS.API.STREAM.PURGE.{EVENT_STREAM}"),
        format!("$JS.API.STREAM.DELETE.{EVENT_STREAM}"),
        format!("$JS.API.CONSUMER.DELETE.{EVENT_STREAM}.ratatoskr_github_analysis_completed"),
    ] {
        let response = timeout(
            REQUEST_TIMEOUT + Duration::from_millis(250),
            github.request(forbidden.clone(), "{}".into()),
        )
        .await;
        assert!(
            !matches!(response, Ok(Ok(_))),
            "GitHub unexpectedly received a response after publishing to {forbidden}"
        );
    }
}
