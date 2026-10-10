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
    EVENT_STREAM, TELEGRAM_NOTIFICATION_CONSUMER, TELEGRAM_NOTIFICATION_SUBJECT,
};
use std::time::Duration;
use tokio::time::timeout;

mod support;

use support::{Container, REQUEST_TIMEOUT, connect};

#[derive(Debug)]
struct NatsFixture {
    container: Container,
    admin_seed: String,
    telegram_seed: String,
    chatgpt_seed: String,
    claude_seed: String,
}

impl NatsFixture {
    fn start(include_telegram_identity: bool) -> Self {
        let admin = KeyPair::new_user();
        let telegram = KeyPair::new_user();
        let chatgpt = KeyPair::new_user();
        let claude = KeyPair::new_user();
        let telegram_public = include_telegram_identity.then(|| telegram.public_key());
        let config = nats_config(
            &admin.public_key(),
            telegram_public.as_deref(),
            &chatgpt.public_key(),
            &claude.public_key(),
        );
        Self {
            container: Container::start("nats-permissions", &config, &[]),
            admin_seed: admin.seed().expect("the disposable admin seed"),
            telegram_seed: telegram.seed().expect("the disposable Telegram seed"),
            chatgpt_seed: chatgpt.seed().expect("the disposable ChatGPT seed"),
            claude_seed: claude.seed().expect("the disposable Claude seed"),
        }
    }

    async fn connect(&self, seed: &str) -> async_nats::Client {
        connect(&self.container.url, seed).await
    }
}

fn nats_config(
    admin_public: &str,
    telegram_public: Option<&str>,
    chatgpt_public: &str,
    claude_public: &str,
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
    let telegram_separator = if telegram_public.is_some() { "," } else { "" };
    format!(
        r#"
port: 4222
host: 0.0.0.0
jetstream {{ store_dir: /data }}
authorization {{
    users: [
        {{ nkey: {admin_public} }},
        {telegram_user}{telegram_separator}
        {{
            nkey: {chatgpt_public}
            permissions: {{
                publish: {{ allow: ["evt.ai-archive.chatgpt.operation.reported.v1"] }}
                subscribe: {{ allow: ["_INBOX.>"] }}
            }}
        }},
        {{
            nkey: {claude_public}
            permissions: {{
                publish: {{ allow: ["evt.ai-archive.claude.operation.reported.v1"] }}
                subscribe: {{ allow: ["_INBOX.>"] }}
            }}
        }}
    ]
}}
"#
    )
}

#[tokio::test]
async fn ai_archive_provider_nkeys_cannot_impersonate_or_subscribe() {
    const CHATGPT: &str = "evt.ai-archive.chatgpt.operation.reported.v1";
    const CLAUDE: &str = "evt.ai-archive.claude.operation.reported.v1";
    let deployed = include_str!("../../../deploy/nats/ratatoskr.conf");
    for required in [
        "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_CHATGPT",
        "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_CLAUDE",
        CHATGPT,
        CLAUDE,
    ] {
        assert!(
            deployed.contains(required),
            "the deployed NATS policy is missing {required}"
        );
    }
    let fixture = NatsFixture::start(false);
    let admin = fixture.connect(&fixture.admin_seed).await;
    let chatgpt = fixture.connect(&fixture.chatgpt_seed).await;
    let claude = fixture.connect(&fixture.claude_seed).await;

    chatgpt
        .publish(CHATGPT, "chatgpt".into())
        .await
        .expect("ChatGPT can publish only its own report subject");
    claude
        .publish(CLAUDE, "claude".into())
        .await
        .expect("Claude can publish only its own report subject");

    for (client, forbidden) in [(&chatgpt, CLAUDE), (&claude, CHATGPT)] {
        let response = timeout(
            REQUEST_TIMEOUT + Duration::from_millis(250),
            client.request(forbidden, "impersonation".into()),
        )
        .await;
        assert!(
            !matches!(response, Ok(Ok(_))),
            "a provider unexpectedly published a foreign report"
        );
        let mut subscription = client
            .subscribe("evt.>")
            .await
            .expect("the client accepts the subscription request before server authorization");
        admin
            .publish(CHATGPT, "private".into())
            .await
            .expect("the admin publishes the denial probe");
        assert!(
            !matches!(
                timeout(Duration::from_millis(250), subscription.next()).await,
                Ok(Some(_))
            ),
            "a provider unexpectedly received a direct event subscription"
        );
    }

    assert!(
        async_nats::connect(&fixture.container.url).await.is_err(),
        "anonymous access must be refused"
    );
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
