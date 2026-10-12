//! Edge erases the stream messages a producer names, and only its own (XR-021 CONTRACTS.md R5-12).
//!
//! The broker runs the deployed ACL, rendered with `platform-nats-profile`: the `ChatGPT` and Claude
//! identities publish the facts and the commands exactly as the importers do, and Edge's identity
//! consumes the command and erases through `STREAM.MSG.GET` and `STREAM.MSG.DELETE`. The database
//! is a real `PostgreSQL`. What is asserted is observed on the stream (the sequence is gone or it
//! is not), in the inbox, in the audit table and in the counters.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::collections::BTreeMap;
use std::time::Duration;

use async_nats::jetstream::{self, stream::RawMessageErrorKind};
use platform_eventing::stream_erasure::{DURABLE, SUBJECT, StreamErasureHandler};
use platform_eventing::{StreamSpec, consumer};
use platform_persistence::test_support::TestDatabase;
use ratatoskr_event_envelope::EnvelopeSchemaVersion;
use sqlx::Row as _;
use uuid::Uuid;

#[allow(
    dead_code,
    reason = "the helpers are shared by several test binaries and each uses part of them"
)]
#[path = "support/recorder.rs"]
mod recorder;
#[allow(
    dead_code,
    reason = "the helpers are shared by several test binaries and each uses part of them"
)]
mod support;

use support::{Container, connect};

const CONF: &str = include_str!("../../../deploy/nats/ratatoskr.conf");
const EVT: &str = "ratatoskr_events";

const CHATGPT: &str = "ratatoskr-chatgpt";
const CLAUDE: &str = "ratatoskr-claude";

const CONVERSATION: &str = "evt.ai_archive.conversation.added.v1";
const TOMBSTONE: &str = "evt.ai_archive.subject.tombstoned.v1";
/// A subject the `ChatGPT` identity may publish that is not under `evt.ai_archive`.
const REPORT: &str = "evt.ai-archive.chatgpt.operation.reported.v1";

fn owner(tail: u32) -> String {
    format!("user:018f0000-0000-7000-8000-{tail:012}")
}

/// The deployed ACL on a real broker, Edge's consumer running, and a database.
struct Harness {
    _container: Container,
    seeds: BTreeMap<String, String>,
    url: String,
    database: TestDatabase,
    context: jetstream::Context,
    consumer_task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start() -> Self {
        let rendered = platform_nats_profile::render(CONF).expect("the deployed ACL renders");
        let seeds: BTreeMap<String, String> = rendered
            .seeds
            .iter()
            .map(|seed| (seed.name.clone(), seed.seed.clone()))
            .collect();
        let container = Container::start("stream-erasure", &rendered.conf, &["--user", "root"]);
        let url = container.url.clone();
        let edge = connect(&url, seeds.get("EDGE").expect("EDGE seed"), None)
            .await
            .0;
        let context = jetstream::new(edge);
        for spec in [StreamSpec::command_stream(), StreamSpec::event_stream()] {
            context
                .get_or_create_stream(spec.config())
                .await
                .expect("EDGE declares the streams");
        }
        let database = TestDatabase::create().await.expect("a test database");
        let pool = database.pool().clone();
        let handler = StreamErasureHandler::new(context.clone());
        let consumer_context = context.clone();
        let consumer_task = tokio::spawn(async move {
            let outcome = consumer::run(
                &consumer_context,
                &StreamSpec::command_stream(),
                DURABLE,
                SUBJECT,
                &pool,
                &handler,
                std::future::pending::<()>(),
            )
            .await;
            panic!("the erasure consumer stopped: {outcome:?}");
        });
        Self {
            _container: container,
            seeds,
            url,
            database,
            context,
            consumer_task,
        }
    }

    /// A client for `identity`, on the inbox prefix its stanza allows.
    async fn client(&self, identity: &str) -> jetstream::Context {
        let prefix = format!("_INBOX_{}", identity.to_ascii_lowercase());
        let seed = self.seeds.get(identity).expect("an identity seed");
        jetstream::new(connect(&self.url, seed, Some(&prefix)).await.0)
    }

    /// Publish a fact as `producer`'s identity for `tenant` and return its stream sequence.
    async fn fact(&self, identity: &str, subject: &str, producer: &str, tenant: &str) -> u64 {
        let body = serde_json::json!({
            "event_id": Uuid::now_v7(),
            "producer": producer,
            "tenant_id": tenant,
            "payload": { "text": "private conversation text" },
        });
        self.client(identity)
            .await
            .publish(subject.to_owned(), body.to_string().into())
            .await
            .expect("the fact is accepted")
            .await
            .expect("the fact is stored")
            .sequence
    }

    /// Publish an erasure command as `identity` and return its command id.
    async fn command(&self, identity: &str, shape: &Command<'_>) -> Uuid {
        let command_id = Uuid::now_v7();
        self.send(identity, command_id, shape).await;
        command_id
    }

    async fn send(&self, identity: &str, command_id: Uuid, shape: &Command<'_>) {
        let request = Uuid::now_v7();
        let envelope = serde_json::json!({
            "command_id": command_id,
            "command_type": "platform.stream_erasure.requested.v1",
            "issued_at": "2026-10-12T12:00:00Z",
            "producer": shape.producer,
            "aggregate_id": shape
                .aggregate
                .map_or_else(|| format!("privacy_request:{request}"), str::to_owned),
            "correlation_id": format!("correlation:{request}"),
            "tenant_id": shape.tenant.unwrap_or(shape.owner),
            "schema_version": serde_json::to_value(EnvelopeSchemaVersion::CURRENT)
                .expect("the envelope version serializes"),
            "payload": {
                "request_ref": format!("privacy_request:{request}"),
                "owner": shape.owner,
                "stream_sequences": shape.sequences,
            },
        });
        let envelope = if shape.aggregate.is_some() {
            envelope
        } else {
            let mut envelope = envelope;
            envelope["aggregate_id"] = envelope["payload"]["request_ref"].clone();
            envelope
        };
        self.client(identity)
            .await
            .publish(SUBJECT.to_owned(), envelope.to_string().into())
            .await
            .expect("the command is accepted")
            .await
            .expect("the command is stored");
    }

    /// Wait until the inbox records the command as handled, and return its outcome.
    async fn handled(&self, command_id: Uuid) -> String {
        for _ in 0..100 {
            let outcome: Option<String> = sqlx::query_scalar(
                "select outcome from operations.inbox
                  where message_id = $1 and processed_at is not null",
            )
            .bind(command_id)
            .fetch_optional(self.database.pool())
            .await
            .expect("an inbox read");
            if let Some(outcome) = outcome {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the command {command_id} was not handled within ten seconds");
    }

    /// The (erased, missing, refused) counts of the audit rows of `command_id`.
    async fn audit(&self, command_id: Uuid) -> Vec<(i32, i32, i32)> {
        sqlx::query(
            "select erased, missing, refused from operations.stream_erasure_audit
              where command_id = $1",
        )
        .bind(command_id)
        .fetch_all(self.database.pool())
        .await
        .expect("an audit read")
        .iter()
        .map(|row| {
            (
                row.get::<i32, _>("erased"),
                row.get::<i32, _>("missing"),
                row.get::<i32, _>("refused"),
            )
        })
        .collect()
    }

    /// Print one line of the erasure transcript: the subject, the sequences and the audit counts,
    /// never a body. `--nocapture` shows it.
    async fn note(&self, label: &str, command_id: Uuid, sequences: &[u64]) {
        let counts = self.audit(command_id).await;
        #[expect(clippy::print_stderr, reason = "the transcript is the test's evidence")]
        {
            eprintln!(
                "erasure transcript: {label}: subject={SUBJECT} sequences={sequences:?} \
                 (erased, missing, refused)={counts:?}"
            );
        }
    }

    /// Whether the stream still holds `sequence`.
    async fn exists(&self, sequence: u64) -> bool {
        let stream = self
            .context
            .get_stream(EVT)
            .await
            .expect("the event stream");
        match stream.get_raw_message(sequence).await {
            Ok(_) => true,
            Err(error) if matches!(error.kind(), RawMessageErrorKind::NoMessageFound) => false,
            Err(error) => panic!("reading sequence {sequence}: {error}"),
        }
    }

    async fn finish(self) {
        self.consumer_task.abort();
        self.database.cleanup().await.expect("cleanup");
    }
}

/// The shape of one erasure command.
struct Command<'a> {
    producer: &'a str,
    owner: &'a str,
    /// The envelope `tenant_id`, when it is not the owner.
    tenant: Option<&'a str>,
    /// The envelope `aggregate_id`, when it is not the request reference.
    aggregate: Option<&'a str>,
    sequences: &'a [u64],
}

impl Harness {
    /// Send a well-formed command from `producer`'s identity for `owner`, wait for it to be handled,
    /// print its transcript line, and return its (erased, missing, refused) counts.
    async fn ask(
        &self,
        label: &str,
        producer: &str,
        owner: &str,
        sequences: &[u64],
    ) -> (i32, i32, i32) {
        let identity = if producer == CLAUDE {
            "CLAUDE"
        } else {
            "CHATGPT"
        };
        let command_id = self
            .command(
                identity,
                &Command {
                    producer,
                    owner,
                    tenant: None,
                    aggregate: None,
                    sequences,
                },
            )
            .await;
        assert_eq!(self.handled(command_id).await, "applied", "{label}");
        self.note(label, command_id, sequences).await;
        let rows = self.audit(command_id).await;
        assert_eq!(rows.len(), 1, "{label}: one audit row per command");
        rows.first().copied().expect("an audit row")
    }
}

#[tokio::test]
async fn a_producer_erases_only_its_own_message_for_the_owner_it_names() {
    let seen = recorder::Recorded::default();
    let sink = seen.recorder();
    let _installed = metrics::set_default_local_recorder(&sink);
    let harness = Harness::start().await;
    let (ada, grace) = (owner(1), owner(2));
    let own = harness.fact("CHATGPT", CONVERSATION, CHATGPT, &ada).await;
    let neighbour = harness.fact("CHATGPT", CONVERSATION, CHATGPT, &ada).await;

    // (a) the producer's own message for the owner it names is erased, its neighbour is not.
    let counts = harness
        .ask("own message erased", CHATGPT, &ada, &[own])
        .await;
    assert_eq!(counts, (1, 0, 0));
    assert!(!harness.exists(own).await, "the named message is erased");
    assert!(harness.exists(neighbour).await, "its neighbour survives");

    // (b) another producer cannot erase it.
    let counts = harness
        .ask("claude names a chatgpt message", CLAUDE, &ada, &[neighbour])
        .await;
    assert_eq!(counts, (0, 0, 1));
    assert!(
        harness.exists(neighbour).await,
        "ChatGPT's message survives"
    );

    // (c) naming another owner erases nothing of this one.
    let counts = harness
        .ask(
            "chatgpt names the wrong owner",
            CHATGPT,
            &grace,
            &[neighbour],
        )
        .await;
    assert_eq!(counts, (0, 0, 1));
    assert!(harness.exists(neighbour).await, "Ada's message survives");

    // (i) the counters carry the outcomes of the three commands.
    for (outcome, expected) in [("erased", 1.0), ("missing", 0.0), ("refused", 2.0)] {
        assert_eq!(
            seen.get(&format!(
                "platform_stream_erasure_total{{outcome={outcome}}}"
            )),
            Some(expected),
            "{outcome}: {:?}",
            seen.series()
        );
    }
    harness.finish().await;
}

#[tokio::test]
async fn a_tombstone_and_a_report_are_never_erased_and_a_missing_sequence_is_skipped() {
    let seen = recorder::Recorded::default();
    let sink = seen.recorder();
    let _installed = metrics::set_default_local_recorder(&sink);
    let harness = Harness::start().await;
    let ada = owner(1);
    let own = harness.fact("CHATGPT", CONVERSATION, CHATGPT, &ada).await;
    let tombstone = harness.fact("CHATGPT", TOMBSTONE, CHATGPT, &ada).await;
    let report = harness.fact("CHATGPT", REPORT, CHATGPT, &ada).await;

    // (d) a tombstone and a message outside `evt.ai_archive` are refused and survive.
    let counts = harness
        .ask("tombstone and report", CHATGPT, &ada, &[tombstone, report])
        .await;
    assert_eq!(counts, (0, 0, 2));
    assert!(
        harness.exists(tombstone).await,
        "a tombstone is never erased"
    );
    assert!(
        harness.exists(report).await,
        "a report is not an archive fact"
    );

    // (e) a sequence that is gone is skipped, and the rest of the command still counts.
    let beyond = report + 1000;
    let counts = harness
        .ask("existing and missing", CHATGPT, &ada, &[own, beyond])
        .await;
    assert_eq!(counts, (1, 1, 0));
    assert!(!harness.exists(own).await, "the existing message goes");

    for (outcome, expected) in [("erased", 1.0), ("missing", 1.0), ("refused", 2.0)] {
        assert_eq!(
            seen.get(&format!(
                "platform_stream_erasure_total{{outcome={outcome}}}"
            )),
            Some(expected),
            "{outcome}: {:?}",
            seen.series()
        );
    }
    harness.finish().await;
}

#[tokio::test]
async fn a_replayed_command_erases_nothing_twice() {
    let seen = recorder::Recorded::default();
    let sink = seen.recorder();
    let _installed = metrics::set_default_local_recorder(&sink);
    let harness = Harness::start().await;
    let ada = owner(1);
    let own = harness.fact("CHATGPT", CONVERSATION, CHATGPT, &ada).await;
    let shape = Command {
        producer: CHATGPT,
        owner: &ada,
        tenant: None,
        aggregate: None,
        sequences: &[own],
    };
    let command_id = harness.command("CHATGPT", &shape).await;
    assert_eq!(harness.handled(command_id).await, "applied");

    // The same command id, delivered again: the inbox absorbs it.
    harness.send("CHATGPT", command_id, &shape).await;
    let sentinel = harness.command("CHATGPT", &shape).await;
    assert_eq!(harness.handled(sentinel).await, "applied");

    assert_eq!(
        harness.audit(command_id).await,
        [(1, 0, 0)],
        "one audit row for the command, however often it was delivered"
    );
    assert_eq!(
        seen.get("platform_stream_erasure_total{outcome=erased}"),
        Some(1.0),
        "{:?}",
        seen.series()
    );
    harness.finish().await;
}

#[tokio::test]
async fn a_command_whose_envelope_contradicts_itself_is_termed() {
    let harness = Harness::start().await;
    let (ada, grace) = (owner(1), owner(2));
    let own = harness.fact("CHATGPT", CONVERSATION, CHATGPT, &ada).await;
    let elsewhere = "privacy_request:018f0000-0000-7000-8000-000000000999";

    let cases = [
        // The envelope tenant is not the payload owner.
        Command {
            producer: CHATGPT,
            owner: &ada,
            tenant: Some(&grace),
            aggregate: None,
            sequences: &[own],
        },
        // A producer that is not an AI archive importer.
        Command {
            producer: "ratatoskr-knowledge",
            owner: &ada,
            tenant: None,
            aggregate: None,
            sequences: &[own],
        },
        // The aggregate is not the request the payload names.
        Command {
            producer: CHATGPT,
            owner: &ada,
            tenant: None,
            aggregate: Some(elsewhere),
            sequences: &[own],
        },
    ];
    for case in &cases {
        let command_id = harness.command("CHATGPT", case).await;
        assert_eq!(harness.handled(command_id).await, "rejected");
        assert!(
            harness.audit(command_id).await.is_empty(),
            "a Termed command leaves no audit row"
        );
        assert!(harness.exists(own).await, "nothing was erased");
    }
    harness.finish().await;
}
