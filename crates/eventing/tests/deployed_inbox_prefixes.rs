//! The deployed `deploy/nats/ratatoskr.conf` and the inbox prefixes (XR-021 CONTRACTS.md R3-01).
//!
//! Every non-Edge identity subscribes only to `_INBOX_<name>.>`, so a compromised service cannot
//! read the replies and deliveries addressed to another. The static test reads the deployed file
//! against the contract table written out below; the broker tests render the same file with
//! `platform-nats-profile` and run it (`support` says how), then observe what each identity is
//! refused. A refused subscription is only visible as a server error delivered to the connection's
//! event callback (and a `Subscription Violation` line in the server log), so that is the channel
//! these tests assert through. The per-identity publish and fetch matrix is `deployed_nats_config.rs`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::collections::BTreeMap;
use std::time::Duration;

use async_nats::jetstream;
use platform_eventing::{StreamSpec, ensure_fixed_topology};
use tokio::time::timeout;

mod support;

use support::{Container, REQUEST_TIMEOUT, ServerErrors, connect};

const CONF: &str = include_str!("../../../deploy/nats/ratatoskr.conf");

const CMD: &str = "ratatoskr_commands";
const EVT: &str = "ratatoskr_events";

/// One durable each identity owns, to describe on its own prefix. CHATGPT and CLAUDE own none.
const OWN_DURABLE: [(&str, &str, &str); 10] = [
    ("TELEGRAM", EVT, "ratatoskr_telegram_notifications"),
    ("X", CMD, "ratatoskr_x_browser_capture"),
    ("INSTAGRAM", CMD, "ratatoskr_instagram_browser_capture"),
    ("THREADS", CMD, "threads_browser_capture"),
    ("EXTRACTOR", CMD, "ratatoskr_extractor_capture"),
    ("EXTRACTOR_BROWSER_WORKER", CMD, "ratatoskr_browser_worker"),
    ("KNOWLEDGE", CMD, "ratatoskr_knowledge_channel_recap"),
    ("GITHUB", EVT, "ratatoskr_github_analysis_completed"),
    ("VAULT", CMD, "ratatoskr_vault_backup_policy"),
    (
        "CHANNEL_DIGESTS",
        CMD,
        "ratatoskr_channel_digest_subscriptions",
    ),
];

/// XR-021 R3-01: each non-Edge identity's inbox prefix, the only namespace it may subscribe to. The
/// prefix is `_INBOX_` and the identity name in lower case, underscores kept. Written out rather
/// than derived, so the table is the contract and the deployed file is what is checked against it.
const INBOX_PREFIXES: [(&str, &str); 12] = [
    ("TELEGRAM", "_INBOX_telegram"),
    ("CHATGPT", "_INBOX_chatgpt"),
    ("CLAUDE", "_INBOX_claude"),
    ("X", "_INBOX_x"),
    ("INSTAGRAM", "_INBOX_instagram"),
    ("THREADS", "_INBOX_threads"),
    ("EXTRACTOR", "_INBOX_extractor"),
    (
        "EXTRACTOR_BROWSER_WORKER",
        "_INBOX_extractor_browser_worker",
    ),
    ("KNOWLEDGE", "_INBOX_knowledge"),
    ("GITHUB", "_INBOX_github"),
    ("VAULT", "_INBOX_vault"),
    ("CHANNEL_DIGESTS", "_INBOX_channel_digests"),
];

/// The prefix an identity connects with: its own, or the library default for EDGE.
fn inbox_prefix(identity: &str) -> Option<&'static str> {
    INBOX_PREFIXES
        .iter()
        .find(|(name, _)| *name == identity)
        .map(|(_, prefix)| *prefix)
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
        // which the unprivileged image user cannot create (`compose.yaml` gives it a uid instead).
        let container = Container::start("deployed-inbox", &rendered.conf, &["--user", "root"]);
        Self { container, seeds }
    }

    /// Connect as `identity` with an explicit inbox prefix and the server errors it receives.
    async fn client_observed(
        &self,
        identity: &str,
        prefix: Option<&str>,
    ) -> (async_nats::Client, ServerErrors) {
        let seed = self
            .seeds
            .get(identity)
            .unwrap_or_else(|| panic!("the deployed ACL has no {identity} identity"));
        connect(&self.container.url, seed, prefix).await
    }

    /// EDGE declares both streams and every fixed durable, as `ratatoskr-edge` does at startup.
    async fn provision(&self) {
        let context = jetstream::new(self.client_observed("EDGE", None).await.0);
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
        .publish(subject.to_owned(), b"{}".to_vec().into())
        .await
        .map_err(|error| error.to_string())?;
    match timeout(REQUEST_TIMEOUT + Duration::from_millis(500), pending).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("no acknowledgement".to_owned()),
    }
}

/// Wait until `errors` holds at least `count` entries or the wait ends, then return them.
async fn server_errors(errors: &ServerErrors, count: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let seen = errors.lock().expect("the server error sink").clone();
        if seen.len() >= count || tokio::time::Instant::now() >= deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// XR-021 R3-01 invariant 7, statically: no non-Edge stanza subscribes to anything but the single
/// entry `<prefix>.>` of its row, so none can read the shared `_INBOX.>` namespace or any other
/// service's replies. NATS compares whole tokens, so `_INBOX_x.>` does not match `_INBOX.>`.
#[test]
fn every_non_edge_stanza_subscribes_only_to_its_own_inbox_prefix() {
    let identities = platform_nats_profile::parse(CONF).expect("the deployed ACL parses");
    assert_eq!(identities.len(), INBOX_PREFIXES.len() + 1);
    for identity in &identities {
        if identity.name == "EDGE" {
            assert_eq!(
                identity.subscribe_allow,
                ["_INBOX.>"],
                "EDGE keeps the shared inbox"
            );
            continue;
        }
        let prefix = inbox_prefix(&identity.name)
            .unwrap_or_else(|| panic!("{} has no row in the prefix table", identity.name));
        assert_eq!(
            identity.subscribe_allow,
            [format!("{prefix}.>")],
            "{} may subscribe to its own inbox prefix only",
            identity.name
        );
    }
}

/// XR-021 R3-01: the shared namespace is closed to every service. A refused subscription is only
/// visible as a server error on the connection (and a `Subscription Violation` in the server log).
#[tokio::test]
async fn a_service_cannot_subscribe_to_the_shared_inbox_namespace() {
    let broker = Deployed::start();
    // The trusted side is the control: its subscription to the shared namespace raises no error.
    let (edge, edge_errors) = broker.client_observed("EDGE", None).await;
    let _edge_subscription = edge.subscribe("_INBOX.>").await.expect("EDGE subscribes");
    edge.flush().await.expect("EDGE flushes");
    for (identity, prefix) in INBOX_PREFIXES {
        let (client, errors) = broker.client_observed(identity, Some(prefix)).await;
        let _subscription = client
            .subscribe("_INBOX.>")
            .await
            .expect("the client accepts the request before server authorization");
        client.flush().await.expect("the flush completes");
        let seen = server_errors(&errors, 1).await;
        assert!(
            seen.iter()
                .any(|error| error
                    .contains("Permissions Violation for Subscription to \"_INBOX.>\"")),
            "{identity} was not refused the shared inbox namespace: {seen:?}"
        );
    }
    assert_eq!(
        server_errors(&edge_errors, 0).await,
        Vec::<String>::new(),
        "EDGE was refused its own inbox namespace"
    );
}

/// XR-021 R3-01: a service cannot read another service's replies either.
#[tokio::test]
async fn a_service_cannot_subscribe_to_another_services_inbox_prefix() {
    let broker = Deployed::start();
    for (identity, prefix) in INBOX_PREFIXES {
        let (client, errors) = broker.client_observed(identity, Some(prefix)).await;
        let foreign: Vec<String> = INBOX_PREFIXES
            .iter()
            .filter(|(name, _)| *name != identity)
            .map(|(_, other)| format!("{other}.>"))
            .collect();
        let mut subscriptions = Vec::new();
        for subject in &foreign {
            subscriptions.push(
                client
                    .subscribe(subject.clone())
                    .await
                    .expect("the request is sent"),
            );
        }
        // Its own prefix is accepted: the last subscription must not raise an error.
        subscriptions.push(
            client
                .subscribe(format!("{prefix}.>"))
                .await
                .expect("the request is sent"),
        );
        client.flush().await.expect("the flush completes");
        let seen = server_errors(&errors, foreign.len()).await;
        for subject in &foreign {
            assert!(
                seen.iter().any(|error| error.contains(&format!(
                    "Permissions Violation for Subscription to \"{subject}\""
                ))),
                "{identity} was not refused {subject}: {seen:?}"
            );
        }
        assert_eq!(
            seen.len(),
            foreign.len(),
            "{identity} was refused its own prefix or something else: {seen:?}"
        );
    }
}

/// One `JetStream` request as `identity`: it describes a durable it owns, or publishes and awaits
/// the stream's acknowledgement when it owns none.
async fn jetstream_request(
    broker: &Deployed,
    identity: &str,
    inbox_prefix: Option<&str>,
) -> Result<(), String> {
    let (client, _errors) = broker.client_observed(identity, inbox_prefix).await;
    let Some((_, stream, durable)) = OWN_DURABLE.iter().find(|(owner, _, _)| *owner == identity)
    else {
        return publish_acked(&client, "evt.ai_archive.archive.imported.v1").await;
    };
    jetstream::new(client)
        .get_consumer_from_stream::<jetstream::consumer::pull::Config, _, _>(*durable, *stream)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// XR-021 R3-01: the refusal does not break the service. On its own prefix every identity completes
/// a `JetStream` request, and on the library default it cannot.
#[tokio::test]
async fn each_service_completes_a_jetstream_request_on_its_own_prefix() {
    let broker = Deployed::start();
    broker.provision().await;
    for (identity, prefix) in INBOX_PREFIXES {
        jetstream_request(&broker, identity, Some(prefix))
            .await
            .unwrap_or_else(|error| {
                panic!("{identity} failed a JetStream request on its own prefix: {error}")
            });
        assert!(
            jetstream_request(&broker, identity, None).await.is_err(),
            "{identity} completed a JetStream request on the default inbox, which the ACL withholds"
        );
    }
}
