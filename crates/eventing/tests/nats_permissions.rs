//! Real-broker proof for the least-privilege Telegram notification identity.

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
    COMMAND_STREAM, EVENT_STREAM, KNOWLEDGE_CHANNEL_RECAP_CONSUMER,
    KNOWLEDGE_CHANNEL_RECAP_SUBJECT, KNOWLEDGE_MAIN_CONSUMER, KNOWLEDGE_MAIN_SUBJECTS,
    KNOWLEDGE_TERMINAL_SUBJECTS, TELEGRAM_NOTIFICATION_CONSUMER, TELEGRAM_NOTIFICATION_SUBJECT,
};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

struct NatsFixture {
    container: String,
    directory: PathBuf,
    url: String,
    admin_seed: String,
    telegram_seed: String,
    knowledge_seed: String,
}

impl NatsFixture {
    fn start(include_telegram_identity: bool, include_knowledge_identity: bool) -> Self {
        let suffix = Uuid::now_v7().simple().to_string();
        let container = format!("ratatoskr-platform-nats-permissions-{suffix}");
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/nats-permission-fixtures")
            .join(&container);
        std::fs::create_dir_all(&directory).expect("the disposable NATS directory");

        let admin = KeyPair::new_user();
        let telegram = KeyPair::new_user();
        let knowledge = KeyPair::new_user();
        let telegram_public = include_telegram_identity.then(|| telegram.public_key());
        let knowledge_public = include_knowledge_identity.then(|| knowledge.public_key());
        let config = nats_config(
            &admin.public_key(),
            telegram_public.as_deref(),
            knowledge_public.as_deref(),
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
            telegram_seed: telegram.seed().expect("the disposable Telegram seed"),
            knowledge_seed: knowledge.seed().expect("the disposable Knowledge seed"),
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
    knowledge_public: Option<&str>,
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
    let knowledge_user = knowledge_public.map_or_else(String::new, |public_key| {
        let event_stream = EVENT_STREAM;
        let main_consumer = KNOWLEDGE_MAIN_CONSUMER;
        let command_stream = COMMAND_STREAM;
        let recap_consumer = KNOWLEDGE_CHANNEL_RECAP_CONSUMER;
        let terminal_subjects = KNOWLEDGE_TERMINAL_SUBJECTS
            .iter()
            .map(|subject| format!("\"{subject}\""))
            .collect::<Vec<_>>()
            .join(",\n                        ");
        format!(
            r#"
        {{
            nkey: {public_key}
            permissions: {{
                publish: {{
                    allow: [
                        "$JS.API.CONSUMER.INFO.{event_stream}.{main_consumer}",
                        "$JS.API.CONSUMER.MSG.NEXT.{event_stream}.{main_consumer}",
                        "$JS.ACK.{event_stream}.{main_consumer}.>",
                        "$JS.API.CONSUMER.INFO.{command_stream}.{recap_consumer}",
                        "$JS.API.CONSUMER.MSG.NEXT.{command_stream}.{recap_consumer}",
                        "$JS.ACK.{command_stream}.{recap_consumer}.>",
                        {terminal_subjects}
                    ]
                }}
                subscribe: {{ allow: ["_INBOX.>"] }}
            }}
        }}"#,
        )
    });
    let admin_separator = if telegram_public.is_some() || knowledge_public.is_some() {
        ","
    } else {
        ""
    };
    let telegram_separator = if telegram_public.is_some() && knowledge_public.is_some() {
        ","
    } else {
        ""
    };
    format!(
        r"
port: 4222
host: 0.0.0.0
jetstream {{ store_dir: /data }}
authorization {{
    users: [
        {{ nkey: {admin_public} }}{admin_separator}
        {telegram_user}{telegram_separator}
        {knowledge_user}
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
    let fixture = NatsFixture::start(true, false);
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

async fn provision_knowledge_fixture(admin: &async_nats::Client) {
    let admin_jetstream = jetstream::new(admin.clone());
    let event_stream = admin_jetstream
        .create_stream(jetstream::stream::Config {
            name: EVENT_STREAM.to_owned(),
            subjects: vec!["evt.>".to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the admin creates the event stream");
    let command_stream = admin_jetstream
        .create_stream(jetstream::stream::Config {
            name: COMMAND_STREAM.to_owned(),
            subjects: vec!["cmd.>".to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the admin creates the command stream");
    event_stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: Some(KNOWLEDGE_MAIN_CONSUMER.to_owned()),
            filter_subjects: KNOWLEDGE_MAIN_SUBJECTS.map(str::to_owned).to_vec(),
            ack_policy: jetstream::consumer::AckPolicy::Explicit,
            ..jetstream::consumer::pull::Config::default()
        })
        .await
        .expect("the admin creates the fixed primary durable");
    command_stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: Some(KNOWLEDGE_CHANNEL_RECAP_CONSUMER.to_owned()),
            filter_subject: KNOWLEDGE_CHANNEL_RECAP_SUBJECT.to_owned(),
            ack_policy: jetstream::consumer::AckPolicy::Explicit,
            ..jetstream::consumer::pull::Config::default()
        })
        .await
        .expect("the admin creates the fixed recap durable");
    event_stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: Some("foreign_knowledge_events".to_owned()),
            filter_subject: KNOWLEDGE_MAIN_SUBJECTS
                .first()
                .expect("the primary inventory is non-empty")
                .to_string(),
            ack_policy: jetstream::consumer::AckPolicy::Explicit,
            ..jetstream::consumer::pull::Config::default()
        })
        .await
        .expect("the admin creates a foreign durable for denial proof");

    let primary_subject = KNOWLEDGE_MAIN_SUBJECTS
        .first()
        .expect("the primary inventory is non-empty");
    for (subject, payload) in [
        (*primary_subject, "primary"),
        (KNOWLEDGE_CHANNEL_RECAP_SUBJECT, "recap"),
    ] {
        admin_jetstream
            .publish(subject, payload.into())
            .await
            .expect("the admin publishes a fixture delivery")
            .await
            .expect("the fixture delivery is stored");
    }
}

#[tokio::test]
async fn knowledge_identity_is_limited_to_fixed_consumers_and_terminal_subjects() {
    let fixture = NatsFixture::start(false, true);
    let admin = fixture.connect(&fixture.admin_seed).await;
    provision_knowledge_fixture(&admin).await;

    let knowledge = fixture.connect(&fixture.knowledge_seed).await;
    let knowledge_jetstream = jetstream::new(knowledge.clone());
    for (stream, durable) in [
        (EVENT_STREAM, KNOWLEDGE_MAIN_CONSUMER),
        (COMMAND_STREAM, KNOWLEDGE_CHANNEL_RECAP_CONSUMER),
    ] {
        let consumer: jetstream::consumer::PullConsumer = knowledge_jetstream
            .get_consumer_from_stream(durable, stream)
            .await
            .unwrap_or_else(|error| panic!("Knowledge can describe {stream}/{durable}: {error}"));
        let mut messages = consumer
            .messages()
            .await
            .unwrap_or_else(|error| panic!("Knowledge can fetch {stream}/{durable}: {error}"));
        let message = timeout(REQUEST_TIMEOUT, messages.next())
            .await
            .unwrap_or_else(|error| panic!("the allowed {durable} fetch timed out: {error}"))
            .unwrap_or_else(|| panic!("the allowed {durable} fetch ended"))
            .unwrap_or_else(|error| panic!("the allowed {durable} delivery is invalid: {error}"));
        message.ack().await.unwrap_or_else(|error| {
            panic!("Knowledge can acknowledge {stream}/{durable}: {error}")
        });
    }

    for subject in KNOWLEDGE_TERMINAL_SUBJECTS {
        knowledge_jetstream
            .publish(subject, "terminal".into())
            .await
            .unwrap_or_else(|error| panic!("Knowledge can publish {subject}: {error}"))
            .await
            .unwrap_or_else(|error| panic!("the broker acknowledges {subject}: {error}"));
    }

    for forbidden in [
        format!("$JS.API.CONSUMER.DURABLE.CREATE.{EVENT_STREAM}.arbitrary"),
        format!("$JS.API.CONSUMER.INFO.{EVENT_STREAM}.foreign_knowledge_events"),
        format!("$JS.API.CONSUMER.MSG.NEXT.{EVENT_STREAM}.foreign_knowledge_events"),
        format!("$JS.API.STREAM.INFO.{EVENT_STREAM}"),
        format!("$JS.API.STREAM.DELETE.{EVENT_STREAM}"),
        "evt.social.source.captured.v1".to_owned(),
    ] {
        let response = timeout(
            REQUEST_TIMEOUT + Duration::from_millis(250),
            knowledge.request(forbidden.clone(), "{}".into()),
        )
        .await;
        assert!(
            !matches!(response, Ok(Ok(_))),
            "Knowledge unexpectedly received a response after publishing to {forbidden}"
        );
    }

    let mut forbidden_subscription = knowledge
        .subscribe("evt.>")
        .await
        .expect("the client can submit the forbidden subscription for broker denial");
    admin
        .publish("evt.social.source.captured.v1", "source".into())
        .await
        .expect("the admin publishes the direct-subscription probe");
    assert!(
        timeout(REQUEST_TIMEOUT, forbidden_subscription.next())
            .await
            .is_err(),
        "Knowledge must not receive a broad direct event subscription"
    );
}
