//! What a stream is allowed to hold, and what it does when it is full.
//!
//! `JetStream`'s own defaults are the reason this file exists. Every unset field of
//! `jetstream::stream::Config` is its zero, and under the default `RetentionPolicy::Limits` those
//! zeros mean "no limit": `max_bytes: 0`, `max_age: 0`, `duplicate_window: 0`. A stream declared
//! from the defaults therefore never removes anything until the store fills — and then, under the
//! default `DiscardPolicy::Old`, silently deletes the OLDEST messages, which are the ones nobody has
//! consumed yet. At-least-once delivery turns into occasionally-never, with no error anywhere.
//!
//! `duplicate_window` is the one field where zero does NOT mean unlimited, and the difference was
//! settled against a running broker rather than assumed: a stream created with `0` reports a window
//! of two minutes, because the server substitutes its own default. Deduplication therefore works
//! without being declared — the risk is only that the window is a property of whichever server the
//! stream was created on, and would change under us if that default did. Stating it costs one line
//! and removes the dependency.
//!
//! Both declaration sites — the publisher's and the consumer's — take a [`StreamSpec`], so a stream
//! cannot be created with one policy by whichever process reached it first.

use std::time::Duration;

use async_nats::jetstream;

use crate::EventingError;

/// How long a redelivery of the same `Nats-Msg-Id` is collapsed by the server.
///
/// Two minutes: comfortably longer than the outbox's bounded backoff, so an in-flight retry is
/// caught by the server, and short enough that the window is not itself a store. It happens to be
/// the server's own default today, which is why leaving it unset works; declaring it is what stops
/// a server-side default from silently becoming our deduplication policy. It is a first line of
/// defence and not the only one — the inbox covers a consumer restarted after the window has
/// passed, which is why `operations.inbox` exists.
const DUPLICATE_WINDOW: Duration = Duration::from_mins(2);

/// What a stream does when it has no room left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenFull {
    /// Refuse the publish. Correct for **commands**: the transactional outbox is the durable copy,
    /// so a refused publish becomes a retry, a bounded backoff and finally a dead-lettered row
    /// carrying its last error — every step of which an operator can see. Dropping the command
    /// instead would lose work that a client was told had been accepted.
    RefusePublish,

    /// Drop the oldest. Correct for **events**: an event is a fact that has already happened, the
    /// producer keeps its own record of it, and a consumer far enough behind to reach the limit has
    /// a problem that retaining more bytes does not fix. Refusing the publish here would push the
    /// failure back into a producer that cannot do anything about it.
    DropOldest,
}

/// One stream, with the limits it is created with.
///
/// Named limits rather than a builder: there are two kinds of stream in this system and each has one
/// correct answer, so a construction that can produce a third is a way to get it wrong.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    /// The stream name.
    pub name: String,
    /// The subjects it is bound to.
    pub subjects: Vec<String>,
    /// The ceiling on stored bytes. Never zero: zero means unlimited.
    pub max_bytes: i64,
    /// How long a message is retained. Never zero: zero means forever.
    pub max_age: Duration,
    /// What happens at the ceiling.
    pub when_full: WhenFull,
}

/// The default ceiling for either stream: 1 GiB.
///
/// A bound, not a target. The messages here are small JSON documents, so a gigabyte is a very deep
/// backlog — deep enough that reaching it means something upstream is broken, which is precisely
/// when a limit should exist. Sized to be safe on the single host of ADR-0013, whose NATS server
/// is given an 8 GiB file store — room for both streams and a wide margin. Raising this is a code
/// change, deliberately: a limit that a deployment can remove is not a limit.
const DEFAULT_MAX_BYTES: i64 = 1024 * 1024 * 1024;

/// The stream every command is published to.
///
/// One stream for `cmd.>` rather than one per command family: a stream is a store with a retention
/// policy, and every command in this system wants the same one. Named here rather than in the
/// binary that declares it, because the NATS permission file in `deploy/nats/` and the operator
/// commands in `deploy/README.md` name the same string, and a name that lives in three places is a
/// name that will eventually differ in one of them.
pub const COMMAND_STREAM: &str = "ratatoskr_commands";

/// The subject filter of [`COMMAND_STREAM`]. ADR-0005 makes the class prefix the privilege
/// boundary, so this is also the publish permission of a role that emits commands.
pub const COMMAND_SUBJECTS: &str = "cmd.>";

/// The stream every event is published to.
pub const EVENT_STREAM: &str = "ratatoskr_events";

/// The subject filter of [`EVENT_STREAM`].
pub const EVENT_SUBJECTS: &str = "evt.>";

/// The durable consumer `ratatoskr-edge` reads operation events through.
///
/// Durable and named, so a restart resumes where the last one stopped instead of replaying the
/// stream or skipping what arrived while the process was down.
pub const EDGE_PROJECTION_CONSUMER: &str = "platform_edge_projection";

/// Whether a fixed durable takes acknowledgements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckMode {
    /// Every message is acknowledged individually. Every durable but the render-await durable.
    Explicit,
    /// No acknowledgements. The render-await durable is read by its single owner, which has no use
    /// for redelivery of a fact it only waits on.
    None,
}

/// Where a fixed durable starts reading a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAt {
    /// From the first retained message, so nothing published before the consumer first runs is lost.
    All,
    /// From the moment the durable is created.
    New,
}

/// One durable consumer that Edge creates before the service that owns it can become ready.
///
/// Every field is stated. A durable created from `pull::Config::default()` would inherit a 30 s ack
/// wait and unlimited redelivery from the server, which are decisions, and a decision that lives in
/// the server's default is one a server upgrade can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedConsumerSpec {
    /// The stream the durable lives on.
    pub stream: &'static str,
    /// The stable durable cursor name, also used by its NATS permission stanza.
    pub durable_name: &'static str,
    /// The sole subject filter this durable may receive.
    pub filter_subject: &'static str,
    /// Whether messages are acknowledged.
    pub ack: AckMode,
    /// Where the durable starts.
    pub start: StartAt,
    /// How long the server waits for an acknowledgement before redelivering.
    pub ack_wait_seconds: u64,
    /// The delivery attempts allowed, or -1 for unlimited.
    pub max_deliver: i64,
}

impl FixedConsumerSpec {
    /// The shape every durable had before XR-021: explicit acknowledgements, deliver all, a 30
    /// second ack wait and unlimited redelivery.
    #[must_use]
    pub const fn standard(
        stream: &'static str,
        durable_name: &'static str,
        filter_subject: &'static str,
    ) -> Self {
        Self {
            stream,
            durable_name,
            filter_subject,
            ack: AckMode::Explicit,
            start: StartAt::All,
            ack_wait_seconds: 30,
            max_deliver: -1,
        }
    }

    /// The same durable with another ack wait.
    #[must_use]
    pub const fn with_ack_wait_seconds(mut self, seconds: u64) -> Self {
        self.ack_wait_seconds = seconds;
        self
    }

    /// The same durable with a bound on delivery attempts.
    #[must_use]
    pub const fn with_max_deliver(mut self, attempts: i64) -> Self {
        self.max_deliver = attempts;
        self
    }

    /// The `JetStream` configuration this spec describes: a pull consumer, replay instant.
    #[must_use]
    pub fn config(&self) -> jetstream::consumer::pull::Config {
        jetstream::consumer::pull::Config {
            durable_name: Some(self.durable_name.to_owned()),
            filter_subject: self.filter_subject.to_owned(),
            ack_policy: match self.ack {
                AckMode::Explicit => jetstream::consumer::AckPolicy::Explicit,
                AckMode::None => jetstream::consumer::AckPolicy::None,
            },
            deliver_policy: match self.start {
                StartAt::All => jetstream::consumer::DeliverPolicy::All,
                StartAt::New => jetstream::consumer::DeliverPolicy::New,
            },
            ack_wait: Duration::from_secs(self.ack_wait_seconds),
            max_deliver: self.max_deliver,
            replay_policy: jetstream::consumer::ReplayPolicy::Instant,
            ..jetstream::consumer::pull::Config::default()
        }
    }
}

/// Provider-scoped archive report consumers owned by Edge.
pub const AI_ARCHIVE_REPORT_CONSUMERS: [FixedConsumerSpec; 2] = [
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "platform_ai_archive_chatgpt_projection",
        "evt.ai-archive.chatgpt.operation.reported.v1",
    ),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "platform_ai_archive_claude_projection",
        "evt.ai-archive.claude.operation.reported.v1",
    ),
];

/// The durable Telegram reads raised notification events through.
pub const TELEGRAM_NOTIFICATION_CONSUMER: &str = "ratatoskr_telegram_notifications";

/// The sole event subject delivered to [`TELEGRAM_NOTIFICATION_CONSUMER`].
pub const TELEGRAM_NOTIFICATION_SUBJECT: &str = "evt.platform.notification.raised.v1";

/// Provider-specific browser-capture consumers pre-provisioned by Platform.
///
/// Social identities can inspect and pull only their own durable. Giving them consumer-create
/// authority would let a compromised identity choose a different filter and observe another
/// provider's commands.
pub const SOCIAL_CAPTURE_CONSUMERS: [FixedConsumerSpec; 3] = [
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_x_browser_capture",
        "cmd.x.capture.requested.v1",
    ),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_instagram_browser_capture",
        "cmd.instagram.capture.requested.v1",
    ),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "threads_browser_capture",
        "cmd.threads.capture.requested.v1",
    ),
];

/// The Telegram notification durable.
pub const TELEGRAM_NOTIFICATION_CONSUMERS: [FixedConsumerSpec; 1] = [FixedConsumerSpec::standard(
    EVENT_STREAM,
    TELEGRAM_NOTIFICATION_CONSUMER,
    TELEGRAM_NOTIFICATION_SUBJECT,
)];

/// The durables of every other service (XR-021 CONTRACTS.md section S04), in the order of the
/// contract: the command stream first, then the event stream.
///
/// Each service hard-codes the same name and filter and verifies them at startup; none creates one.
/// Ingest durables wait 120 seconds because an LLM call sits between delivery and acknowledgement.
pub const DOMAIN_CONSUMERS: [FixedConsumerSpec; 18] = [
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_extractor_capture",
        "cmd.content.capture.requested.v1",
    )
    .with_max_deliver(12),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_browser_worker",
        "cmd.content.render.requested.v1",
    )
    .with_ack_wait_seconds(300)
    .with_max_deliver(12),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_knowledge_channel_recap",
        "cmd.knowledge.channel_digest_recap.requested.v1",
    ),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_channel_digest_subscriptions",
        "cmd.channel_digest.subscription.set_requested.v1",
    ),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_channel_digest_runs",
        "cmd.channel_digest.run.requested.v1",
    ),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_channel_digest_schedule_occurrences",
        "cmd.channel_digest.schedule.occurrence_requested.v1",
    ),
    FixedConsumerSpec::standard(
        COMMAND_STREAM,
        "ratatoskr_vault_backup_policy",
        "cmd.vault.backup_policy.apply_requested.v1",
    ),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_knowledge_documents",
        "evt.content.document.extracted.v1",
    )
    .with_ack_wait_seconds(120),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_knowledge_social_sources",
        "evt.social.source.>",
    )
    .with_ack_wait_seconds(120),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_knowledge_ai_archive",
        "evt.ai_archive.>",
    )
    .with_ack_wait_seconds(120),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_knowledge_repository_requests",
        "evt.knowledge.repository_analysis.requested.v1",
    )
    .with_ack_wait_seconds(120),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_github_analysis_completed",
        "evt.knowledge.repository_analysis.completed.v1",
    ),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_github_analysis_failed",
        "evt.knowledge.repository_analysis.failed.v1",
    ),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_github_policy_acknowledged",
        "evt.vault.backup_policy.acknowledged.v1",
    ),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_x_extractor_reports",
        "evt.platform.operation.reported.v1",
    ),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_channel_digest_recap_completed",
        "evt.knowledge.channel_digest_recap.completed.v1",
    ),
    FixedConsumerSpec::standard(
        EVENT_STREAM,
        "ratatoskr_channel_digest_recap_failed",
        "evt.knowledge.channel_digest_recap.failed.v1",
    ),
    FixedConsumerSpec {
        stream: EVENT_STREAM,
        durable_name: "ratatoskr_extractor_render_awaits",
        filter_subject: "evt.content.render.>",
        ack: AckMode::None,
        start: StartAt::New,
        ack_wait_seconds: 30,
        max_deliver: -1,
    },
];

/// The KV bucket the browser worker records render completions in, created by Edge.
pub const BROWSER_WORKER_COMPLETIONS_BUCKET: &str = "browser_worker_completions";

/// How long a completion marker lives.
const BROWSER_WORKER_COMPLETIONS_MAX_AGE: Duration = Duration::from_hours(24);

/// The default retention: seven days.
///
/// Long enough that a broker outage over a weekend does not lose an event, short enough that the
/// store is not an archive. Nothing reads a week-old command: the outbox would have dead-lettered it
/// long before.
///
/// Derived from `platform_core::config::EVENT_RETENTION_DAYS` rather than written here, because the
/// inbox retention window must not be shorter than it — startup rule V17 — and a second copy of the
/// number is how the two would come to disagree. A test asserts they are still the same value.
const DEFAULT_MAX_AGE: Duration =
    Duration::from_hours(24 * platform_core::config::EVENT_RETENTION_DAYS);

impl StreamSpec {
    /// A command stream, which refuses a publish rather than dropping work.
    #[must_use]
    pub fn commands(name: impl Into<String>, subjects: Vec<String>) -> Self {
        Self {
            name: name.into(),
            subjects,
            max_bytes: DEFAULT_MAX_BYTES,
            max_age: DEFAULT_MAX_AGE,
            when_full: WhenFull::RefusePublish,
        }
    }

    /// The command stream this deployment publishes to, with the name and subjects of the profile.
    #[must_use]
    pub fn command_stream() -> Self {
        Self::commands(COMMAND_STREAM, vec![COMMAND_SUBJECTS.to_owned()])
    }

    /// The event stream this deployment consumes from.
    #[must_use]
    pub fn event_stream() -> Self {
        Self::events(EVENT_STREAM, vec![EVENT_SUBJECTS.to_owned()])
    }

    /// An event stream, which drops the oldest rather than refusing a fact.
    #[must_use]
    pub fn events(name: impl Into<String>, subjects: Vec<String>) -> Self {
        Self {
            name: name.into(),
            subjects,
            max_bytes: DEFAULT_MAX_BYTES,
            max_age: DEFAULT_MAX_AGE,
            when_full: WhenFull::DropOldest,
        }
    }

    /// The `JetStream` configuration this spec describes.
    ///
    /// Every field that matters is stated. `..Default::default()` is deliberately absent: the whole
    /// point of this type is that no policy field is left to a default whose zero means "unlimited".
    #[must_use]
    pub fn config(&self) -> jetstream::stream::Config {
        jetstream::stream::Config {
            name: self.name.clone(),
            subjects: self.subjects.clone(),
            retention: jetstream::stream::RetentionPolicy::Limits,
            storage: jetstream::stream::StorageType::File,
            discard: match self.when_full {
                WhenFull::RefusePublish => jetstream::stream::DiscardPolicy::New,
                WhenFull::DropOldest => jetstream::stream::DiscardPolicy::Old,
            },
            max_bytes: self.max_bytes,
            max_age: self.max_age,
            duplicate_window: DUPLICATE_WINDOW,
            // One node, so one copy. Stated rather than defaulted so that raising it is a decision
            // somebody makes, on the day there is a second node to put a replica on.
            num_replicas: 1,
            ..jetstream::stream::Config::default()
        }
    }
}

/// What [`ensure`] found on the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamState {
    /// The stream did not exist and was created with exactly these limits.
    Created,
    /// The stream already existed. `mismatches` names every policy field whose stored value differs
    /// from the spec — empty when the two agree.
    Existing {
        /// The differing fields, by name, in a stable order.
        mismatches: Vec<&'static str>,
    },
}

/// Create the stream `spec` describes, or report how the existing one differs.
///
/// `get_or_create_stream` does not reconcile: handed a configuration for a stream that already
/// exists, it returns the existing one and says nothing about the difference. That silence is the
/// same failure class this module exists to prevent — a stream created once from
/// `Config::default()` keeps `max_bytes: -1`, `max_age: 0` and `DiscardPolicy::Old` forever, and
/// every later deployment carrying the correct limits reports success while changing nothing.
///
/// So the difference is computed and returned. Not refused: a looser limit works correctly until
/// the store fills, so turning it into a failed startup would trade a slow problem for an immediate
/// outage. The caller logs it, and an operator updates or deletes the stream — the mismatch is a
/// state on the broker, and a redeploy is not what fixes it.
///
/// # Errors
///
/// [`EventingError::Bus`] if the stream can be neither created nor described.
pub async fn ensure(
    context: &jetstream::Context,
    spec: &StreamSpec,
) -> Result<StreamState, EventingError> {
    let existed = context.get_stream(&spec.name).await.is_ok();

    let stream = context
        .get_or_create_stream(spec.config())
        .await
        .map_err(|error| EventingError::Bus(error.to_string()))?;

    if !existed {
        return Ok(StreamState::Created);
    }

    let mut stream = stream;
    let stored = stream
        .info()
        .await
        .map_err(|error| EventingError::Bus(error.to_string()))?
        .config
        .clone();
    let wanted = spec.config();

    let mut mismatches = Vec::new();
    if stored.max_bytes != wanted.max_bytes {
        mismatches.push("max_bytes");
    }
    if stored.max_age != wanted.max_age {
        mismatches.push("max_age");
    }
    if stored.discard != wanted.discard {
        mismatches.push("discard");
    }
    if stored.duplicate_window != wanted.duplicate_window {
        mismatches.push("duplicate_window");
    }
    if stored.retention != wanted.retention {
        mismatches.push("retention");
    }
    Ok(StreamState::Existing { mismatches })
}

/// Ensures every durable in `specs` exists on its stream with exactly the configuration it states.
///
/// A durable that exists is read, never modified: a cursor is state, and changing its filter or
/// delivery policy under a running service would change which messages it receives without anyone
/// having decided it. A durable that differs from its spec is therefore a startup error for the
/// operator to resolve, and the existing one is left in place.
///
/// # Errors
///
/// Returns [`EventingError::Bus`] when a stream or consumer cannot be read or created, or when an
/// existing durable does not match its spec.
pub async fn ensure_fixed_consumers(
    context: &jetstream::Context,
    specs: &[FixedConsumerSpec],
) -> Result<(), EventingError> {
    for spec in specs {
        let stream = context
            .get_stream(spec.stream)
            .await
            .map_err(|error| EventingError::Bus(error.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(spec.durable_name, spec.config())
            .await
            .map_err(|error| EventingError::Bus(error.to_string()))?;
        let found = &consumer.cached_info().config;
        let wanted = spec.config();
        if found.durable_name != wanted.durable_name
            || found.filter_subject != wanted.filter_subject
            || found.ack_policy != wanted.ack_policy
            || found.deliver_policy != wanted.deliver_policy
            || found.ack_wait != wanted.ack_wait
            || found.max_deliver != wanted.max_deliver
            || found.replay_policy != wanted.replay_policy
            || found.deliver_subject.is_some()
        {
            return Err(EventingError::Bus(format!(
                "the pre-provisioned consumer {} on {} does not match its fixed filter and \
                 delivery policy",
                spec.durable_name, spec.stream
            )));
        }
    }
    Ok(())
}

/// Ensures the KV bucket the browser worker records completions in exists.
///
/// `create_key_value` is idempotent for an identical configuration and an error for a different
/// one, which is the behaviour wanted: a bucket with other limits is left for an operator.
async fn ensure_completion_bucket(context: &jetstream::Context) -> Result<(), EventingError> {
    context
        .create_key_value(jetstream::kv::Config {
            bucket: BROWSER_WORKER_COMPLETIONS_BUCKET.to_owned(),
            description: "Render completions recorded by the extractor browser worker".to_owned(),
            max_age: BROWSER_WORKER_COMPLETIONS_MAX_AGE,
            ..jetstream::kv::Config::default()
        })
        .await
        .map_err(|error| EventingError::Bus(error.to_string()))?;
    Ok(())
}

/// Ensures the durables of the other services and the browser worker's completion bucket exist
/// (CONTRACTS.md section S04).
///
/// # Errors
///
/// Returns [`EventingError::Bus`] when a durable or the bucket cannot be read or created, or when an
/// existing durable does not match its spec.
pub async fn ensure_domain_topology(context: &jetstream::Context) -> Result<(), EventingError> {
    ensure_fixed_consumers(context, &DOMAIN_CONSUMERS).await?;
    ensure_completion_bucket(context).await
}

/// Ensures every fixed durable of every table, and the KV bucket, exist. Edge calls this once at
/// startup, after the two stream declarations, and it is the only place a durable is created.
///
/// # Errors
///
/// Returns [`EventingError::Bus`] when any durable or the bucket cannot be read or created, or when
/// an existing durable does not match its spec.
pub async fn ensure_fixed_topology(context: &jetstream::Context) -> Result<(), EventingError> {
    ensure_fixed_consumers(context, &SOCIAL_CAPTURE_CONSUMERS).await?;
    ensure_fixed_consumers(context, &TELEGRAM_NOTIFICATION_CONSUMERS).await?;
    ensure_fixed_consumers(context, &AI_ARCHIVE_REPORT_CONSUMERS).await?;
    ensure_domain_topology(context).await
}
