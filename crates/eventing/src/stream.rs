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

/// The default retention: seven days.
///
/// Long enough that a broker outage over a weekend does not lose an event, short enough that the
/// store is not an archive. Nothing reads a week-old command: the outbox would have dead-lettered it
/// long before.
const DEFAULT_MAX_AGE: Duration = Duration::from_hours(24 * 7);

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
