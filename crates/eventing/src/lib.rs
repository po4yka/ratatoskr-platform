//! The transactional outbox, the inbox, and the NATS subject grammar.
//!
//! Milestone 4. `ARCHITECTURE.md` S5.1 writes the outbox row in the SAME transaction as the state
//! change that justifies it, so the bus and the database cannot disagree about whether something
//! happened. A publisher then moves rows to the bus **at least once**, which is why every consumer
//! deduplicates through [`inbox`] rather than trusting delivery.
//!
//! The four things this crate refuses to conflate:
//!
//! * **Enqueuing and publishing.** Enqueuing is synchronous, transactional and cannot fail
//!   independently of the caller's transaction. Publishing is asynchronous, retried and may fail
//!   forever. Putting them in one function is how a failed broker becomes a failed HTTP request.
//! * **A duplicate and a failure.** At-least-once delivery makes redelivery ordinary traffic.
//! * **A retry and a dead letter.** Retrying forever hides a poison message; dropping it loses work.
//!   `AGENTS.md` requires a diagnosable dead-letter path, so an exhausted row stays, with its last
//!   error and attempt count, and stops being claimable.
//! * **A subject and a message type.** The subject carries a class prefix so a credential can be
//!   granted publish rights over commands without also granting them over events (`ARCHITECTURE.md`
//!   S15, S18). See ADR-0005.

use platform_persistence::PersistenceError;

pub mod command;
pub mod consumer;
pub mod inbox;
pub mod outbox;
pub mod publisher;
pub mod pump;
pub mod stream;

pub use crate::command::Command;
pub use crate::consumer::{ConsumerReport, Handler, Incoming, deliver};
pub use crate::inbox::{Inbox, Reception};
pub use crate::outbox::{ClaimedMessage, Outbox, OutboxStats};
pub use crate::publisher::{NatsPublisher, PublishError, Publisher};
pub use crate::pump::{PumpReport, run_once};
pub use crate::stream::{StreamSpec, StreamState, WhenFull};

/// A failure in the outbox, the inbox, or the subject grammar.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EventingError {
    /// The string is not a legal subject.
    #[error("{0} is not a valid subject: {1}")]
    InvalidSubject(String, &'static str),

    /// The payload could not be serialized or read back.
    #[error("the message payload could not be converted")]
    Payload(#[source] serde_json::Error),

    /// The bus refused or was unreachable. A string rather than the client's error type, because
    /// that type is not `Clone` and this one crosses a task boundary.
    #[error("the bus could not be reached: {0}")]
    Bus(String),

    /// The database refused or failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// What a message is for, and the first token of its subject.
///
/// The split exists so a NATS credential can be granted `cmd.>` without `evt.>`, which is what
/// `ARCHITECTURE.md` S18 means by "limited command publish permissions". A single flat namespace
/// would make that distinction unexpressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// A request for work that may be rejected. Not a fact.
    Command,
    /// A completed fact. Past tense, never rejected.
    Event,
}

impl MessageClass {
    /// The subject prefix.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "cmd",
            Self::Event => "evt",
        }
    }
}

/// A validated NATS subject.
///
/// The grammar is `<class>.<contract type name>`, where the contract type name is the
/// `<context>.<aggregate>.<action>.v<major>` string `ratatoskr-contracts` already governs. Composing
/// rather than inventing means the bus topology and the contract catalogue cannot drift: a subject
/// that no contract type corresponds to is unconstructible.
///
/// The same grammar is a CHECK constraint on `operations.outbox.subject`. A subject is a security
/// boundary, so it is validated where it is used and again where it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subject(String);

impl Subject {
    /// Every legal character of one token, and the shape of the whole.
    const MAX_LEN: usize = 160;

    /// Build a subject for a contract type name.
    ///
    /// # Errors
    ///
    /// [`EventingError::InvalidSubject`] if the type name is not in the contract grammar.
    pub fn new(class: MessageClass, type_name: &str) -> Result<Self, EventingError> {
        let invalid = |why: &'static str| EventingError::InvalidSubject(type_name.to_owned(), why);

        let Some((body, version)) = type_name.rsplit_once(".v") else {
            return Err(invalid("a contract type name ends with .v<major>"));
        };
        if version.is_empty()
            || version.len() > 3
            || !version.bytes().all(|byte| byte.is_ascii_digit())
            || version.starts_with('0')
        {
            return Err(invalid("the major version is 1..=999 with no leading zero"));
        }

        let segments: Vec<&str> = body.split('.').collect();
        if !(2..=4).contains(&segments.len()) {
            return Err(invalid(
                "a contract type name has two to four segments before .v",
            ));
        }
        for segment in &segments {
            if !is_token(segment) {
                return Err(invalid("every segment is ^[a-z][a-z0-9_]{0,31}$"));
            }
        }

        let subject = format!("{}.{type_name}", class.as_str());
        if subject.len() > Self::MAX_LEN {
            return Err(invalid("the subject is longer than 160 characters"));
        }
        Ok(Self(subject))
    }

    /// Parse a stored or received subject back.
    ///
    /// # Errors
    ///
    /// [`EventingError::InvalidSubject`] if it is not in the grammar.
    pub fn parse(raw: &str) -> Result<Self, EventingError> {
        let invalid = |why: &'static str| EventingError::InvalidSubject(raw.to_owned(), why);
        let Some((class, rest)) = raw.split_once('.') else {
            return Err(invalid("a subject begins with cmd. or evt."));
        };
        let class = match class {
            "cmd" => MessageClass::Command,
            "evt" => MessageClass::Event,
            _ => return Err(invalid("a subject begins with cmd. or evt.")),
        };
        Self::new(class, rest)
    }

    /// The wire form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// What class this subject belongs to.
    #[must_use]
    pub fn class(&self) -> MessageClass {
        if self.0.starts_with("cmd.") {
            MessageClass::Command
        } else {
            MessageClass::Event
        }
    }
}

impl core::fmt::Display for Subject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_token(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || segment.len() > 32 {
        return false;
    }
    bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

pub(crate) fn to_offset(value: jiff::Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(value.as_nanosecond())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

pub(crate) fn from_offset(value: time::OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::from_nanosecond(value.unix_timestamp_nanos())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::{MessageClass, Subject};

    #[test]
    fn a_contract_type_name_becomes_a_classed_subject() {
        let subject = Subject::new(MessageClass::Command, "content.capture.requested.v1")
            .expect("a valid type name");
        assert_eq!(subject.as_str(), "cmd.content.capture.requested.v1");
        assert_eq!(subject.class(), MessageClass::Command);
    }

    #[test]
    fn the_grammar_rejects_what_the_contract_grammar_rejects() {
        for name in [
            "Content.capture.requested.v1", // upper case
            "content.capture.requested",    // no major
            "content.v1",                   // one segment
            "content.capture.requested.v0", // zero major
            "content.capture.requested.v01",
            "content..requested.v1",
            "content.capture.requested.vX",
        ] {
            assert!(
                Subject::new(MessageClass::Event, name).is_err(),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn a_subject_round_trips_through_parse() {
        for raw in [
            "cmd.content.capture.requested.v1",
            "evt.platform.operation.progressed.v1",
        ] {
            let parsed = Subject::parse(raw).expect("a valid subject");
            assert_eq!(parsed.as_str(), raw);
        }
        assert!(Subject::parse("content.capture.requested.v1").is_err());
        assert!(Subject::parse("sys.content.capture.requested.v1").is_err());
    }
}
