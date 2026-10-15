//! The fixed durables of the notes write path (workspace change `add-notes-write-back`, spec
//! `fleet-bus-topology`): the durable through which notes reads the write command, which Edge
//! provisions with the domain table, and the durable through which Edge's own consumer projects the
//! conflict events onto the write operations. And the four durables of the sites service (workspace
//! change `add-site-publications`), and Edge's durable of the site event stream (workspace change
//! `add-site-owner-editing-and-live`).

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::time::Duration;

use async_nats::jetstream;
use platform_eventing::inbox::Outcome;
use platform_eventing::{
    AckMode, COMMAND_STREAM, DOMAIN_CONSUMERS, EDGE_INTERNAL_CONSUMERS, EVENT_STREAM,
    EventingError, FixedConsumerSpec, Handler, Incoming, StartAt, StreamSpec,
};
use platform_persistence::test_support::TestDatabase;

#[allow(
    dead_code,
    reason = "the helpers are shared by several test binaries and each uses part of them"
)]
mod support;

/// A broker with no authentication, so the test can create and read everything.
const OPEN_BROKER: &str = "port: 4222\nhost: 0.0.0.0\njetstream { store_dir: /data }\n";

fn named(table: &[FixedConsumerSpec], durable: &str) -> Option<FixedConsumerSpec> {
    table
        .iter()
        .find(|spec| spec.durable_name == durable)
        .copied()
}

/// Workspace OBS-024 task 10.7.
#[test]
fn notes_write_durables_are_provisioned() {
    assert_eq!(
        named(&DOMAIN_CONSUMERS, "ratatoskr_notes_write_requests"),
        Some(FixedConsumerSpec {
            stream: COMMAND_STREAM,
            durable_name: "ratatoskr_notes_write_requests",
            filter_subject: "cmd.notes.file.write_requested.v1",
            also_filter_subject: None,
            ack: AckMode::Explicit,
            start: StartAt::All,
            ack_wait_seconds: 30,
            max_deliver: -1,
        })
    );
    assert_eq!(
        named(
            &EDGE_INTERNAL_CONSUMERS,
            "platform_notes_conflict_projection"
        ),
        Some(FixedConsumerSpec {
            stream: EVENT_STREAM,
            durable_name: "platform_notes_conflict_projection",
            filter_subject: "evt.notes.conflict.>",
            also_filter_subject: None,
            ack: AckMode::Explicit,
            start: StartAt::All,
            ack_wait_seconds: 30,
            max_deliver: -1,
        }),
        "Edge's own consumers create the notes conflict projection"
    );
}

/// Workspace SITE-025 task 6.12 (spec `fleet-bus-topology`, "The sites durables are provisioned").
#[test]
fn sites_durables_are_provisioned() {
    let row = |stream, durable, filter, ack_wait_seconds| FixedConsumerSpec {
        stream,
        durable_name: durable,
        filter_subject: filter,
        also_filter_subject: None,
        ack: AckMode::Explicit,
        start: StartAt::All,
        ack_wait_seconds,
        max_deliver: -1,
    };
    for expected in [
        row(
            COMMAND_STREAM,
            "ratatoskr_sites_release_builds",
            "cmd.sites.release.build_requested.v1",
            300,
        ),
        row(
            EVENT_STREAM,
            "ratatoskr_sites_note_changes",
            "evt.notes.file.changed.v1",
            120,
        ),
        row(
            EVENT_STREAM,
            "ratatoskr_sites_vault_disconnections",
            "evt.notes.vault.disconnected.v1",
            30,
        ),
        row(
            EVENT_STREAM,
            "ratatoskr_sites_approvals",
            "evt.platform.site_approval.changed.v1",
            30,
        ),
    ] {
        assert_eq!(
            named(&DOMAIN_CONSUMERS, expected.durable_name),
            Some(expected),
            "Edge provisions {}",
            expected.durable_name
        );
    }
    assert_eq!(
        DOMAIN_CONSUMERS.len(),
        30,
        "26 rows after OBS-024, plus the four sites durables"
    );
}

/// A handler that applies nothing: the test only needs the durable that the consumer creates.
struct Nothing;

impl Handler for Nothing {
    async fn handle(
        &self,
        _transaction: &mut sqlx::PgTransaction<'_>,
        _message: &Incoming,
        _now: jiff::Timestamp,
    ) -> Result<Outcome, EventingError> {
        Ok(Outcome::Applied)
    }
}

/// Workspace SITE-027 task 7.3 (spec `fleet-bus-topology`, "The site events durable is
/// provisioned"): on a fresh broker, Edge's consumer of the site event stream creates
/// `platform_site_events` on the events stream with both filters, an ack wait of 30 seconds and
/// explicit acknowledgement.
#[tokio::test]
async fn site_events_durable_is_provisioned() {
    let spec = named(&EDGE_INTERNAL_CONSUMERS, "platform_site_events");
    assert!(
        spec.is_some(),
        "platform_site_events is one of Edge's own consumers"
    );
    let spec = spec.unwrap();

    let container = support::Container::start("site-events", OPEN_BROKER, &["--user", "root"]);
    let mut attempts = 0;
    let client = loop {
        match async_nats::connect(&container.url).await {
            Ok(client) => break client,
            Err(error) if attempts < 100 => {
                attempts += 1;
                let _ = error;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("the fresh broker accepts a client: {error}"),
        }
    };
    let context = jetstream::new(client);
    let harness = TestDatabase::create().await.expect("a test database");
    let filters = spec.filters();
    let events = StreamSpec::event_stream();
    let run = platform_eventing::consumer::run_filters(
        &context,
        &events,
        spec.durable_name,
        &filters,
        harness.pool(),
        &Nothing,
        tokio::time::sleep(Duration::from_secs(1)),
    );
    run.await.expect("Edge's consumer runs");

    let mut consumer = context
        .get_stream(EVENT_STREAM)
        .await
        .expect("the events stream")
        .get_consumer::<jetstream::consumer::pull::Config>("platform_site_events")
        .await
        .expect("the durable exists");
    let info = consumer.info().await.expect("the consumer info");
    let mut filters = info.config.filter_subjects.clone();
    if !info.config.filter_subject.is_empty() {
        filters.push(info.config.filter_subject.clone());
    }
    filters.sort();
    assert_eq!(
        filters,
        [
            "evt.sites.draft.changed.v1",
            "evt.sites.release.published.v1"
        ],
        "the two filters of the durable"
    );
    assert_eq!(info.config.ack_wait, Duration::from_secs(30));
    assert_eq!(
        info.config.ack_policy,
        jetstream::consumer::AckPolicy::Explicit
    );
    assert_eq!(spec.ack_wait_seconds, 30);
    assert_eq!(spec.ack, AckMode::Explicit);

    harness.cleanup().await.expect("cleanup");
}
