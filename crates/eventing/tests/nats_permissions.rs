//! Real-broker proof for the least-privilege Telegram notification identity.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use async_nats::jetstream;
use futures_util::StreamExt as _;
use nkeys::KeyPair;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use platform_eventing::{
    COMMAND_STREAM, COMMAND_SUBJECTS, EVENT_STREAM, TELEGRAM_NOTIFICATION_CONSUMER,
    TELEGRAM_NOTIFICATION_SUBJECT,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct NatsFixture {
    container: String,
    directory: PathBuf,
    url: String,
    admin_seed: String,
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
        let telegram = KeyPair::new_user();
        let telegram_public = include_telegram_identity.then(|| telegram.public_key());
        let config = nats_config(&admin.public_key(), telegram_public.as_deref());
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

fn nats_config(admin_public: &str, telegram_public: Option<&str>) -> String {
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
    let admin_separator = if telegram_public.is_some() { "," } else { "" };
    format!(
        r"
port: 4222
host: 0.0.0.0
jetstream {{ store_dir: /data }}
authorization {{
    users: [
        {{ nkey: {admin_public} }}{admin_separator}
        {telegram_user}
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

#[derive(Debug)]
struct ActualConfigFixture {
    admin_seed_path: PathBuf,
    container: String,
    directory: PathBuf,
    threads_seed_path: PathBuf,
    url: String,
}

impl ActualConfigFixture {
    fn start() -> Self {
        let suffix = Uuid::now_v7().simple().to_string();
        let container = format!("ratatoskr-platform-actual-nats-permissions-{suffix}");
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/nats-permission-fixtures")
            .join(&container);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .expect("the private disposable NATS directory");
        let (admin_seed_path, threads_seed_path) = materialize_actual_config(&directory);

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
            .expect("docker must start NATS with the actual deployment policy");
        assert!(
            started.status.success(),
            "actual-policy NATS fixture failed to start: {}",
            String::from_utf8_lossy(&started.stderr),
        );

        let port = Command::new("docker")
            .args(["port", &container, "4222/tcp"])
            .output()
            .expect("docker must report the actual-policy NATS port");
        if !port.status.success() {
            let logs = Command::new("docker")
                .args(["logs", &container])
                .output()
                .expect("docker must report why actual-policy NATS exited");
            let _ = Command::new("docker")
                .args(["rm", "--force", &container])
                .output();
            let _ = std::fs::remove_dir_all(&directory);
            panic!(
                "docker did not report the actual-policy NATS port: {}{}",
                String::from_utf8_lossy(&port.stderr),
                String::from_utf8_lossy(&logs.stderr),
            );
        }
        let binding = String::from_utf8(port.stdout).expect("the port binding is UTF-8");
        let port = binding
            .trim()
            .rsplit_once(':')
            .map(|(_, port)| port)
            .expect("the port binding has a port");

        Self {
            admin_seed_path,
            container,
            directory,
            threads_seed_path,
            url: format!("nats://127.0.0.1:{port}"),
        }
    }

    async fn connect(&self, seed_path: &Path) -> async_nats::Client {
        let seed = std::fs::read_to_string(seed_path).expect("the disposable seed is readable");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match async_nats::ConnectOptions::with_nkey(seed.trim().to_owned())
                .request_timeout(Some(REQUEST_TIMEOUT))
                .connect(&self.url)
                .await
            {
                Ok(client) => return client,
                Err(error) if tokio::time::Instant::now() < deadline => {
                    let _ = error;
                    sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    panic!("the disposable actual-policy identity did not connect: {error}")
                }
            }
        }
    }
}

fn materialize_actual_config(directory: &Path) -> (PathBuf, PathBuf) {
    let admin = KeyPair::new_user();
    let telegram = KeyPair::new_user();
    let x = KeyPair::new_user();
    let instagram = KeyPair::new_user();
    let threads = KeyPair::new_user();
    let identities = [
        (
            "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_EDGE_XXXXXXXXXX",
            &admin,
        ),
        (
            "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_TELEGRAM_XXXX",
            &telegram,
        ),
        (
            "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_X_XXXXXXXXXXXXX",
            &x,
        ),
        (
            "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_INSTAGRAM_XXXXX",
            &instagram,
        ),
        (
            "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_THREADS_XXXXXXX",
            &threads,
        ),
    ];
    let source_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/nats/ratatoskr.conf");
    let mut config =
        std::fs::read_to_string(source_path).expect("the checked-in deployment NATS configuration");
    for (placeholder, key_pair) in identities {
        config = config.replace(placeholder, &key_pair.public_key());
    }
    config = config.replace("host: 127.0.0.1", "host: 0.0.0.0");

    let config_path = directory.join("nats.conf");
    write_private_file(&config_path, config.as_bytes());
    let admin_seed_path = directory.join("admin.seed");
    write_private_file(
        &admin_seed_path,
        admin.seed().expect("the disposable admin seed").as_bytes(),
    );
    let threads_seed_path = directory.join("threads.seed");
    write_private_file(
        &threads_seed_path,
        threads
            .seed()
            .expect("the disposable Threads seed")
            .as_bytes(),
    );

    for path in [&config_path, &admin_seed_path, &threads_seed_path] {
        assert_private_file(path);
    }

    (admin_seed_path, threads_seed_path)
}

fn write_private_file(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .expect("the private fixture file is created once");
    file.write_all(bytes)
        .expect("the private fixture file is written");
}

fn assert_private_file(path: &Path) {
    let mode = std::fs::metadata(path)
        .expect("the private fixture file metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "fixture files containing policy or seeds stay private"
    );
}

async fn assert_jetstream_publish_denied(context: &jetstream::Context, subject: &str) {
    let outcome = timeout(REQUEST_TIMEOUT + Duration::from_millis(250), async {
        let acknowledgement = context.publish(subject.to_owned(), "denied".into()).await?;
        acknowledgement.await
    })
    .await;
    assert!(
        !matches!(outcome, Ok(Ok(_))),
        "Threads unexpectedly received a publish acknowledgement for {subject}",
    );
}

async fn assert_request_denied(client: &async_nats::Client, subject: String) {
    let response = timeout(
        REQUEST_TIMEOUT + Duration::from_millis(250),
        client.request(subject.clone(), "{}".into()),
    )
    .await;
    assert!(
        !matches!(response, Ok(Ok(_))),
        "Threads unexpectedly received a response after publishing to {subject}",
    );
}

async fn provision_threads_acl_fixture(context: &jetstream::Context) {
    let command_stream = context
        .create_stream(jetstream::stream::Config {
            name: COMMAND_STREAM.to_owned(),
            subjects: vec![COMMAND_SUBJECTS.to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the admin creates the command stream");
    context
        .create_stream(jetstream::stream::Config {
            name: EVENT_STREAM.to_owned(),
            subjects: vec!["evt.>".to_owned()],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the admin creates the event stream");
    command_stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: Some("threads_browser_capture".to_owned()),
            filter_subject: "cmd.threads.capture.requested.v1".to_owned(),
            ack_policy: jetstream::consumer::AckPolicy::Explicit,
            ..jetstream::consumer::pull::Config::default()
        })
        .await
        .expect("the admin creates the fixed Threads durable");
    context
        .publish("cmd.threads.capture.requested.v1", "capture request".into())
        .await
        .expect("the admin sends the command fixture")
        .await
        .expect("the command stream stores the fixture");
}

async fn assert_threads_command_consumer(context: &jetstream::Context) {
    let consumer: jetstream::consumer::PullConsumer = context
        .get_consumer_from_stream("threads_browser_capture", COMMAND_STREAM)
        .await
        .expect("Threads can describe only its fixed durable");
    let mut messages = consumer
        .messages()
        .await
        .expect("Threads can fetch from its fixed durable");
    let message = timeout(REQUEST_TIMEOUT, messages.next())
        .await
        .expect("the allowed command fetch returns promptly")
        .expect("the fixture command is delivered")
        .expect("the delivered command is valid");
    message
        .ack()
        .await
        .expect("Threads can acknowledge its command");
}

async fn assert_threads_denials(
    context: &jetstream::Context,
    threads: &async_nats::Client,
    admin: &async_nats::Client,
) {
    for subject in [
        "evt.social.source.unowned.v1",
        "cmd.threads.capture.requested.v1",
    ] {
        assert_jetstream_publish_denied(context, subject).await;
    }
    assert_request_denied(
        threads,
        format!("$JS.API.CONSUMER.DURABLE.CREATE.{COMMAND_STREAM}.arbitrary"),
    )
    .await;

    if let Ok(mut subscription) = threads.subscribe("evt.>").await {
        threads
            .flush()
            .await
            .expect("the denied subscription reaches NATS");
        admin
            .publish("evt.social.source.captured.v1", "probe".into())
            .await
            .expect("the admin publishes the direct-subscription probe");
        admin.flush().await.expect("the probe reaches NATS");
        assert!(
            timeout(REQUEST_TIMEOUT, subscription.next()).await.is_err(),
            "Threads unexpectedly received a directly subscribed fleet event",
        );
    }
}

impl Drop for ActualConfigFixture {
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
async fn threads_identity_can_publish_removed_and_nothing_broader() {
    let fixture = ActualConfigFixture::start();
    let admin = fixture.connect(&fixture.admin_seed_path).await;
    let admin_jetstream = jetstream::new(admin.clone());
    provision_threads_acl_fixture(&admin_jetstream).await;

    let threads = fixture.connect(&fixture.threads_seed_path).await;
    let threads_jetstream = jetstream::new(threads.clone());
    assert_threads_command_consumer(&threads_jetstream).await;

    for subject in [
        "evt.platform.operation.reported.v1",
        "evt.social.source.captured.v1",
        "evt.social.source.updated.v1",
    ] {
        timeout(REQUEST_TIMEOUT, async {
            threads_jetstream
                .publish(subject.to_owned(), subject.as_bytes().to_vec().into())
                .await
                .expect("Threads sends an existing owned fact")
                .await
        })
        .await
        .expect("the existing fact acknowledgement is finite")
        .expect("the event stream acknowledges the existing owned fact");
    }

    assert_threads_denials(&threads_jetstream, &threads, &admin).await;

    timeout(REQUEST_TIMEOUT, async {
        threads_jetstream
            .publish(
                "evt.social.source.removed.v1",
                "evt.social.source.removed.v1".into(),
            )
            .await
            .expect("Threads sends its removal fact")
            .await
    })
    .await
    .expect("the removal acknowledgement is finite")
    .expect("the event stream acknowledges the removal fact");

    drop(threads_jetstream);
    drop(threads);
    drop(admin_jetstream);
    drop(admin);
    let directory = fixture.directory.clone();
    drop(fixture);
    assert!(
        !directory.exists(),
        "the seed-bearing fixture directory is removed"
    );
}
