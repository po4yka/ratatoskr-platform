//! Real-broker proof for the least-privilege Telegram notification identity, and for the `NOTES`
//! identity of the deployed ACL with the two Knowledge durables that read note events and the
//! conflict events of the notes write path, and for the `SITES` identity of the site publications.

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
    COMMAND_STREAM, DOMAIN_CONSUMERS, EVENT_STREAM, StreamSpec, TELEGRAM_NOTIFICATION_CONSUMER,
    TELEGRAM_NOTIFICATION_SUBJECT, ensure_fixed_topology,
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

    /// Connect on the inbox prefix the identity's stanza allows (`None` for the unrestricted admin).
    async fn connect(&self, seed: &str, inbox_prefix: Option<&str>) -> async_nats::Client {
        connect(&self.container.url, seed, inbox_prefix).await.0
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
                subscribe: {{ allow: ["_INBOX_telegram.>"] }}
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
                subscribe: {{ allow: ["_INBOX_chatgpt.>"] }}
            }}
        }},
        {{
            nkey: {claude_public}
            permissions: {{
                publish: {{ allow: ["evt.ai-archive.claude.operation.reported.v1"] }}
                subscribe: {{ allow: ["_INBOX_claude.>"] }}
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
    let admin = fixture.connect(&fixture.admin_seed, None).await;
    let chatgpt = fixture
        .connect(&fixture.chatgpt_seed, Some("_INBOX_chatgpt"))
        .await;
    let claude = fixture
        .connect(&fixture.claude_seed, Some("_INBOX_claude"))
        .await;

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
    let admin = fixture.connect(&fixture.admin_seed, None).await;
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

    let telegram = fixture
        .connect(&fixture.telegram_seed, Some("_INBOX_telegram"))
        .await;
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

/// Workspace OBS-023 task 9.5: on a broker that runs the rendered deployed ACL, `NOTES` publishes
/// its events and `KNOWLEDGE` reads them from its two note durables, which Edge provisions.
#[tokio::test]
async fn notes_identity_publishes_its_events_and_knowledge_reads_its_durables() {
    const DURABLES: [&str; 2] = [
        "ratatoskr_knowledge_notes_files",
        "ratatoskr_knowledge_notes_vaults",
    ];
    let rendered =
        platform_nats_profile::render(include_str!("../../../deploy/nats/ratatoskr.conf"))
            .expect("the deployed ACL renders");
    let seed = |identity: &str| {
        rendered
            .seeds
            .iter()
            .find(|seed| seed.name == identity)
            .map(|seed| seed.seed.clone())
    };
    let tabled: Vec<&str> = DURABLES
        .into_iter()
        .filter(|durable| {
            DOMAIN_CONSUMERS
                .iter()
                .any(|spec| spec.durable_name == *durable)
        })
        .collect();
    assert!(
        seed("NOTES").is_some() && tabled == DURABLES,
        "the deployed ACL must hold NOTES (holds it: {}) and Edge must provision {DURABLES:?} \
         (provisions: {tabled:?})",
        seed("NOTES").is_some()
    );

    let broker = Container::start("notes-permissions", &rendered.conf, &["--user", "root"]);
    let edge = connect(&broker.url, &seed("EDGE").expect("the EDGE seed"), None)
        .await
        .0;
    let provisioning = jetstream::new(edge);
    for spec in [StreamSpec::command_stream(), StreamSpec::event_stream()] {
        provisioning
            .get_or_create_stream(spec.config())
            .await
            .expect("EDGE creates the streams");
    }
    ensure_fixed_topology(&provisioning)
        .await
        .expect("EDGE provisions every fixed durable");

    let notes = connect(
        &broker.url,
        &seed("NOTES").expect("the NOTES seed"),
        Some("_INBOX_notes"),
    )
    .await
    .0;
    for subject in ["evt.notes.file.changed.v1", "evt.notes.vault.connected.v1"] {
        let acknowledged = timeout(REQUEST_TIMEOUT + Duration::from_millis(500), async {
            jetstream::new(notes.clone())
                .publish(subject, "{}".into())
                .await
                .map_err(|error| error.to_string())?
                .await
                .map_err(|error| error.to_string())
        })
        .await;
        assert!(
            matches!(acknowledged, Ok(Ok(_))),
            "NOTES could not publish {subject}: {acknowledged:?}"
        );
    }

    let knowledge = connect(
        &broker.url,
        &seed("KNOWLEDGE").expect("the KNOWLEDGE seed"),
        Some("_INBOX_knowledge"),
    )
    .await
    .0;
    for durable in DURABLES {
        let consumer: jetstream::consumer::PullConsumer = jetstream::new(knowledge.clone())
            .get_consumer_from_stream(durable, EVENT_STREAM)
            .await
            .unwrap_or_else(|error| panic!("KNOWLEDGE cannot describe {durable}: {error}"));
        let mut batch = consumer
            .fetch()
            .max_messages(1)
            .expires(Duration::from_secs(2))
            .messages()
            .await
            .unwrap_or_else(|error| panic!("KNOWLEDGE cannot fetch from {durable}: {error}"));
        let message = batch
            .next()
            .await
            .unwrap_or_else(|| panic!("{durable} delivered nothing"))
            .unwrap_or_else(|error| panic!("{durable} delivery: {error}"));
        message
            .double_ack()
            .await
            .unwrap_or_else(|error| panic!("KNOWLEDGE cannot acknowledge on {durable}: {error}"));
    }
}

/// Publish `subject` as `client` and wait for the stream's acknowledgement. `Err` is the observable
/// form of a refusal: a denied publish is never acknowledged.
async fn acknowledged(client: &async_nats::Client, subject: &str) -> Result<(), String> {
    timeout(REQUEST_TIMEOUT + Duration::from_millis(500), async {
        jetstream::new(client.clone())
            .publish(subject.to_owned(), "{}".into())
            .await
            .map_err(|error| error.to_string())?
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| format!("{subject} was not acknowledged in time"))?
}

/// Workspace OBS-024 task 10.9: on a broker that runs the rendered deployed ACL, `NOTES` publishes
/// the two conflict events and no command; only EDGE publishes the write command.
#[tokio::test]
async fn notes_publishes_conflicts_and_no_command() {
    const COMMAND: &str = "cmd.notes.file.write_requested.v1";
    let rendered =
        platform_nats_profile::render(include_str!("../../../deploy/nats/ratatoskr.conf"))
            .expect("the deployed ACL renders");
    let seed = |identity: &str| {
        rendered
            .seeds
            .iter()
            .find(|seed| seed.name == identity)
            .map_or_else(
                || panic!("the deployed ACL has no {identity} seed"),
                |seed| seed.seed.clone(),
            )
    };
    let broker = Container::start("notes-conflicts", &rendered.conf, &["--user", "root"]);
    let edge = connect(&broker.url, &seed("EDGE"), None).await.0;
    let provisioning = jetstream::new(edge.clone());
    for spec in [StreamSpec::command_stream(), StreamSpec::event_stream()] {
        provisioning
            .get_or_create_stream(spec.config())
            .await
            .expect("EDGE creates the streams");
    }
    ensure_fixed_topology(&provisioning)
        .await
        .expect("EDGE provisions every fixed durable");
    let notes = connect(&broker.url, &seed("NOTES"), Some("_INBOX_notes"))
        .await
        .0;

    for subject in [
        "evt.notes.conflict.raised.v1",
        "evt.notes.conflict.resolved.v1",
    ] {
        let outcome = acknowledged(&notes, subject).await;
        assert!(
            outcome.is_ok(),
            "NOTES could not publish {subject}: {outcome:?}"
        );
    }
    assert!(
        acknowledged(&notes, COMMAND).await.is_err(),
        "NOTES published the write command"
    );
    let outcome = acknowledged(&edge, COMMAND).await;
    assert!(
        outcome.is_ok(),
        "EDGE could not publish the write command: {outcome:?}"
    );
}

/// Workspace SITE-025 task 6.12 (spec `fleet-bus-topology`, "The sites service tries to publish a
/// command"): on a broker that runs the rendered deployed ACL, `SITES` reads its four durables, which
/// Edge provisions and EDGE and NOTES feed, publishes its two events, and is refused the build
/// command that only EDGE publishes.
#[tokio::test]
async fn sites_identity_reads_its_durables_and_publishes_only_its_events() {
    const COMMAND: &str = "cmd.sites.release.build_requested.v1";
    // (durable, stream, the identity that feeds it, a subject that matches its filter)
    const DURABLES: [(&str, &str, &str, &str); 4] = [
        (
            "ratatoskr_sites_release_builds",
            COMMAND_STREAM,
            "EDGE",
            COMMAND,
        ),
        (
            "ratatoskr_sites_note_changes",
            EVENT_STREAM,
            "NOTES",
            "evt.notes.file.changed.v1",
        ),
        (
            "ratatoskr_sites_vault_disconnections",
            EVENT_STREAM,
            "NOTES",
            "evt.notes.vault.disconnected.v1",
        ),
        (
            "ratatoskr_sites_approvals",
            EVENT_STREAM,
            "EDGE",
            "evt.platform.site_approval.changed.v1",
        ),
    ];
    let rendered =
        platform_nats_profile::render(include_str!("../../../deploy/nats/ratatoskr.conf"))
            .expect("the deployed ACL renders");
    let seed = |identity: &str| {
        rendered
            .seeds
            .iter()
            .find(|seed| seed.name == identity)
            .map(|seed| seed.seed.clone())
    };
    assert!(
        seed("SITES").is_some(),
        "SITES cannot connect: the deployed ACL has no SITES identity"
    );
    let broker = Container::start("sites-permissions", &rendered.conf, &["--user", "root"]);
    let edge = connect(&broker.url, &seed("EDGE").expect("the EDGE seed"), None)
        .await
        .0;
    let provisioning = jetstream::new(edge.clone());
    for spec in [StreamSpec::command_stream(), StreamSpec::event_stream()] {
        provisioning
            .get_or_create_stream(spec.config())
            .await
            .expect("EDGE creates the streams");
    }
    ensure_fixed_topology(&provisioning)
        .await
        .expect("EDGE provisions every fixed durable");
    let notes = connect(
        &broker.url,
        &seed("NOTES").expect("the NOTES seed"),
        Some("_INBOX_notes"),
    )
    .await
    .0;
    for (durable, _, feeder, subject) in DURABLES {
        let client = if feeder == "EDGE" { &edge } else { &notes };
        let outcome = acknowledged(client, subject).await;
        assert!(
            outcome.is_ok(),
            "{feeder} could not feed {durable} with {subject}: {outcome:?}"
        );
    }

    let sites = connect(
        &broker.url,
        &seed("SITES").expect("the SITES seed"),
        Some("_INBOX_sites"),
    )
    .await
    .0;
    for (durable, stream, _, _) in DURABLES {
        fetch_one_and_acknowledge(&sites, "SITES", stream, durable).await;
    }
    // SITE-027: the sites service publishes the draft change that Edge's site event stream reads.
    for subject in [
        "evt.sites.release.published.v1",
        "evt.platform.operation.reported.v1",
        "evt.sites.draft.changed.v1",
    ] {
        let outcome = acknowledged(&sites, subject).await;
        assert!(
            outcome.is_ok(),
            "SITES could not publish {subject}: {outcome:?}"
        );
    }
    assert!(
        acknowledged(&sites, COMMAND).await.is_err(),
        "SITES published the build command"
    );
}

/// Describe `durable` on `stream` as `identity`, fetch one message and acknowledge it, or panic with
/// the step that `identity` may not take.
async fn fetch_one_and_acknowledge(
    client: &async_nats::Client,
    identity: &str,
    stream: &str,
    durable: &str,
) {
    let consumer: jetstream::consumer::PullConsumer = jetstream::new(client.clone())
        .get_consumer_from_stream(durable, stream)
        .await
        .unwrap_or_else(|error| panic!("{identity} cannot describe {durable}: {error}"));
    let mut batch = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .unwrap_or_else(|error| panic!("{identity} cannot fetch from {durable}: {error}"));
    let message = batch
        .next()
        .await
        .unwrap_or_else(|| panic!("{durable} delivered nothing"))
        .unwrap_or_else(|error| panic!("{durable} delivery: {error}"));
    message
        .double_ack()
        .await
        .unwrap_or_else(|error| panic!("{identity} cannot acknowledge on {durable}: {error}"));
}
