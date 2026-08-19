//! What a stream is created with — tests S-1 … S-4.
//!
//! `jetstream::stream::Config::default()` is the defect these guard against. Every unset field is
//! its zero, and under `RetentionPolicy::Limits` those zeros mean "no limit": a stream declared from
//! the defaults retains everything until the store fills and then, under `DiscardPolicy::Old`,
//! silently deletes the oldest messages — which are exactly the ones nobody has consumed. There is
//! no error anywhere; at-least-once delivery quietly becomes occasionally-never.
//!
//! A zero here is therefore never "unset". These tests assert that each policy field carries a
//! decision, so a future `..Default::default()` fails rather than degrades.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::time::Duration;

use async_nats::jetstream;
use platform_eventing::{NatsPublisher, StreamSpec, StreamState, WhenFull};
use uuid::Uuid;

/// The broker the suite talks to. Matches `compose.yaml`.
#[expect(
    clippy::disallowed_methods,
    reason = "a test binary choosing which broker to talk to"
)]
fn nats_url() -> String {
    std::env::var("PLATFORM_TEST_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_owned())
}

/// S-1. No policy field is left at a default whose zero means "unlimited".
///
/// Asserted field by field rather than by comparing whole structs: a struct comparison would have to
/// be rewritten every time `async-nats` adds a field, and would then be rewritten by pasting in
/// whatever the new default is — which is the failure mode this test exists to prevent.
#[test]
fn every_limit_that_matters_is_stated() {
    for spec in [
        StreamSpec::commands("s", vec!["cmd.>".to_owned()]),
        StreamSpec::events("s", vec!["evt.>".to_owned()]),
    ] {
        let config = spec.config();
        assert!(config.max_bytes > 0, "max_bytes 0 means unlimited");
        assert!(
            config.max_age > Duration::ZERO,
            "max_age 0 means retained forever"
        );
        assert!(
            config.duplicate_window > Duration::ZERO,
            "the deduplication window is ours to state, not the server default's to choose"
        );
        assert_eq!(config.num_replicas, 1);
        assert_eq!(config.retention, jetstream::stream::RetentionPolicy::Limits);
        assert_eq!(config.storage, jetstream::stream::StorageType::File);
    }
}

/// S-2. The asymmetry between the two kinds of stream, which is the whole decision.
///
/// A command stream refuses a publish when it is full, because the outbox is the durable copy and a
/// refusal becomes a retry, a bounded backoff and finally a dead-lettered row an operator can read.
/// An event stream drops its oldest, because an event is a fact its producer already recorded and a
/// consumer far enough behind to hit the limit is not helped by keeping more of them.
#[test]
fn commands_refuse_and_events_drop() {
    let commands = StreamSpec::commands("s", vec!["cmd.>".to_owned()]);
    let events = StreamSpec::events("s", vec!["evt.>".to_owned()]);

    assert_eq!(commands.when_full, WhenFull::RefusePublish);
    assert_eq!(events.when_full, WhenFull::DropOldest);
    assert_eq!(
        commands.config().discard,
        jetstream::stream::DiscardPolicy::New
    );
    assert_eq!(
        events.config().discard,
        jetstream::stream::DiscardPolicy::Old
    );
}

/// S-3. The broker agrees. The unit tests above describe what we ask for; this is what we get.
///
/// A uniquely named stream on a unique subject, because `get_or_create_stream` does NOT reconcile an
/// existing stream against the configuration it is handed — a stream created earlier with different
/// limits keeps them and says nothing. Reusing a shared name here would assert the history of the
/// broker rather than the behaviour of this code.
#[tokio::test]
async fn the_broker_creates_the_stream_with_the_stated_limits() {
    let publisher = NatsPublisher::connect(&nats_url())
        .await
        .expect("a broker; docker compose up -d");
    let name = format!("t_{}", Uuid::now_v7().simple());
    let spec = StreamSpec::commands(&name, vec![format!("{name}.>")]);

    assert_eq!(
        publisher.ensure_stream(&spec).await.expect("declaring"),
        StreamState::Created,
        "a name nothing has used must be a creation, not a silent reuse"
    );

    let info = publisher
        .context()
        .get_stream(&name)
        .await
        .expect("the stream")
        .info()
        .await
        .expect("its info")
        .clone();

    assert_eq!(info.config.max_bytes, spec.max_bytes);
    assert_eq!(info.config.max_age, spec.max_age);
    assert_eq!(info.config.duplicate_window, spec.config().duplicate_window);
    assert_eq!(
        info.config.discard,
        jetstream::stream::DiscardPolicy::New,
        "a command stream must refuse rather than drop"
    );

    publisher
        .context()
        .delete_stream(&name)
        .await
        .expect("cleaning up");
}

/// S-4. A full command stream refuses the publish instead of eating an unconsumed message.
///
/// The behaviour the asymmetry buys, exercised rather than asserted about. With `DiscardPolicy::Old`
/// the second publish would succeed and the first message would be gone — a command a client was
/// told had been accepted, deleted with no error anywhere.
#[tokio::test]
async fn a_full_command_stream_refuses_rather_than_deletes() {
    let publisher = NatsPublisher::connect(&nats_url())
        .await
        .expect("a broker; docker compose up -d");
    let name = format!("t_{}", Uuid::now_v7().simple());
    let mut spec = StreamSpec::commands(&name, vec![format!("{name}.>")]);
    // Small enough that the second message cannot fit.
    spec.max_bytes = 4096;
    publisher.ensure_stream(&spec).await.expect("declaring");

    let context = publisher.context();
    let payload = vec![b'x'; 2048];
    let first = context
        .publish(format!("{name}.one"), payload.clone().into())
        .await
        .expect("sending")
        .await;
    assert!(first.is_ok(), "the first message fits: {first:?}");

    let second = context
        .publish(format!("{name}.two"), payload.into())
        .await
        .expect("sending")
        .await;
    assert!(
        second.is_err(),
        "a full command stream must refuse the publish, not silently drop the first message"
    );

    let messages = context
        .get_stream(&name)
        .await
        .expect("the stream")
        .info()
        .await
        .expect("its info")
        .state
        .messages;
    assert_eq!(messages, 1, "the stored message survived the refusal");

    context.delete_stream(&name).await.expect("cleaning up");
}

/// S-5. A stream that already exists is not reconciled, and the difference is REPORTED.
///
/// The failure this closes was found by running the service rather than by reading it: the limits
/// were correct in the code, the streams on the broker had been created earlier from
/// `Config::default()`, and every subsequent start reported success while `max_bytes: -1`,
/// `max_age: 0` and `DiscardPolicy::Old` stayed exactly as they were. Silence is what made it
/// invisible, so the difference is now returned and the caller warns.
#[tokio::test]
async fn an_existing_stream_with_other_limits_is_reported_not_silently_accepted() {
    let publisher = NatsPublisher::connect(&nats_url())
        .await
        .expect("a broker; docker compose up -d");
    let name = format!("t_{}", Uuid::now_v7().simple());

    // Created the way a pre-milestone-7 deployment created it: every limit left to its default.
    publisher
        .context()
        .create_stream(jetstream::stream::Config {
            name: name.clone(),
            subjects: vec![format!("{name}.>")],
            ..jetstream::stream::Config::default()
        })
        .await
        .expect("the legacy stream");

    let state = publisher
        .ensure_stream(&StreamSpec::commands(&name, vec![format!("{name}.>")]))
        .await
        .expect("declaring");

    let StreamState::Existing { mismatches } = state else {
        panic!("an existing stream must not be reported as created: {state:?}");
    };
    // The three that matter, and the three the legacy defaults actually got wrong. NOT
    // `duplicate_window`: a stream created with `0` reports the server's default of two minutes,
    // which is the value this spec asks for anyway, so there is nothing to report. Measured here
    // rather than assumed — it is the one field in this configuration whose zero does not mean
    // "unlimited", and an earlier draft of this module claimed the opposite.
    for field in ["max_bytes", "max_age", "discard"] {
        assert!(
            mismatches.contains(&field),
            "{field} differs and must be named: {mismatches:?}"
        );
    }

    publisher
        .context()
        .delete_stream(&name)
        .await
        .expect("cleaning up");
}
