//! Consumer lag and stream state, read from a real broker (XR-021 CONTRACTS.md R5-13).
//!
//! A throwaway unauthenticated broker holds the two streams and one durable. Messages are published,
//! pulled without acknowledging and acknowledged one by one, and after each step the sampler runs
//! under a test recorder with an injected clock. The age is asserted against the message's own
//! stream timestamp, never against the wall clock.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::time::Duration;

use async_nats::jetstream::{self, consumer::pull};
use futures_util::StreamExt as _;
use jiff::Timestamp;
use platform_eventing::observe::sample_bus;
use platform_eventing::{EVENT_STREAM, FixedConsumerSpec, StreamSpec};

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

use support::Container;

const DURABLE: &str = "ratatoskr_knowledge_documents";
const FILTER: &str = "evt.content.document.extracted.v1";
const SUBJECT: &str = "evt.content.document.extracted.v1";

/// A broker with no authentication, so the test can create and read everything.
const OPEN_BROKER: &str = "port: 4222\nhost: 0.0.0.0\njetstream { store_dir: /data }\n";

fn spec() -> FixedConsumerSpec {
    FixedConsumerSpec::standard(EVENT_STREAM, DURABLE, FILTER).with_ack_wait_seconds(120)
}

fn series(name: &str) -> String {
    format!("{name}{{durable={DURABLE},stream={EVENT_STREAM}}}")
}

fn stream_series(name: &str) -> String {
    format!("{name}{{stream={EVENT_STREAM}}}")
}

/// The instant of a stream message, as the broker stamped it.
fn stamped(message: &jetstream::message::StreamMessage) -> Timestamp {
    Timestamp::from_nanosecond(message.time.unix_timestamp_nanos()).expect("a message timestamp")
}

struct Broker {
    _container: Container,
    context: jetstream::Context,
    stream: jetstream::stream::Stream,
}

async fn broker() -> Broker {
    let container = Container::start("consumer-lag", OPEN_BROKER, &["--user", "root"]);
    // The server needs a moment to listen after the process starts.
    let mut attempts = 0;
    let client = loop {
        match async_nats::connect(&container.url).await {
            Ok(client) => break client,
            Err(error) if attempts < 100 => {
                attempts += 1;
                let _ = error;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("the throwaway broker accepts a client: {error}"),
        }
    };
    let context = jetstream::new(client);
    for stream in [StreamSpec::command_stream(), StreamSpec::event_stream()] {
        context
            .get_or_create_stream(stream.config())
            .await
            .expect("a stream is declared");
    }
    let stream = context
        .get_stream(EVENT_STREAM)
        .await
        .expect("the event stream");
    Broker {
        _container: container,
        context,
        stream,
    }
}

async fn publish(broker: &Broker, count: usize) {
    for index in 0..count {
        broker
            .context
            .publish(SUBJECT.to_owned(), format!("{{\"n\":{index}}}").into())
            .await
            .expect("a publish")
            .await
            .expect("a stored message");
        // The stream timestamps have a resolution the test can tell apart.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Acknowledge the message still in flight and every message that is left.
async fn acknowledge_everything(
    consumer: &jetstream::consumer::PullConsumer,
    in_flight: jetstream::Message,
) {
    in_flight
        .double_ack()
        .await
        .expect("the second is acknowledged");
    let mut rest = consumer
        .fetch()
        .max_messages(10)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .expect("a fetch");
    while let Some(message) = rest.next().await {
        message
            .expect("a delivery")
            .double_ack()
            .await
            .expect("an acknowledgement");
    }
}

#[tokio::test]
async fn lag_rises_and_falls_with_acknowledgements() {
    let broker = broker().await;
    let consumer = broker
        .stream
        .create_consumer(pull::Config {
            durable_name: Some(DURABLE.to_owned()),
            filter_subject: FILTER.to_owned(),
            ack_wait: Duration::from_mins(2),
            ..pull::Config::default()
        })
        .await
        .expect("the durable");
    publish(&broker, 5).await;
    let seen = recorder::Recorded::default();
    let sink = seen.recorder();
    let _installed = metrics::set_default_local_recorder(&sink);
    let specs = [spec()];

    // Nothing pulled: five waiting, none in flight, the oldest unacked is the first message.
    let first = broker.stream.get_raw_message(1).await.expect("sequence 1");
    let now: Timestamp = stamped(&first)
        .checked_add(jiff::SignedDuration::from_secs(600))
        .expect("an instant");
    sample_bus(&broker.context, &specs, now).await;
    assert_eq!(
        seen.get(&series("platform_bus_consumer_pending")),
        Some(5.0)
    );
    assert_eq!(
        seen.get(&series("platform_bus_consumer_ack_pending")),
        Some(0.0)
    );
    let age = seen
        .get(&series("platform_bus_consumer_oldest_unacked_age_seconds"))
        .expect("an age");
    assert!(
        (age - 600.0).abs() < 0.001,
        "the first message is ten minutes old: {age}"
    );

    // Two pulled, one acknowledged.
    let mut batch = consumer
        .fetch()
        .max_messages(2)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .expect("a fetch");
    let one = batch.next().await.expect("a message").expect("a delivery");
    let two = batch.next().await.expect("a message").expect("a delivery");
    one.double_ack().await.expect("the first is acknowledged");
    let mut info = consumer.clone();
    let server = info.info().await.expect("consumer info").clone();
    let second = broker.stream.get_raw_message(2).await.expect("sequence 2");
    let now = stamped(&second)
        .checked_add(jiff::SignedDuration::from_secs(3600))
        .expect("an instant");
    sample_bus(&broker.context, &specs, now).await;

    #[expect(
        clippy::cast_precision_loss,
        reason = "small counts compared as gauge values"
    )]
    let waiting = server.num_pending as f64;
    assert_eq!(
        seen.get(&series("platform_bus_consumer_pending")),
        Some(waiting)
    );
    assert_eq!(
        waiting.to_bits(),
        3.0_f64.to_bits(),
        "the server reports three waiting"
    );
    assert_eq!(
        seen.get(&series("platform_bus_consumer_ack_pending")),
        Some(1.0)
    );
    let age = seen
        .get(&series("platform_bus_consumer_oldest_unacked_age_seconds"))
        .expect("an age");
    assert!(
        (age - 3600.0).abs() < 0.001,
        "the oldest unacked is the second message, an hour old by its own timestamp: {age}"
    );

    // Everything acknowledged: caught up.
    acknowledge_everything(&consumer, two).await;
    sample_bus(&broker.context, &specs, now).await;
    assert_eq!(
        seen.get(&series("platform_bus_consumer_pending")),
        Some(0.0)
    );
    assert_eq!(
        seen.get(&series("platform_bus_consumer_ack_pending")),
        Some(0.0)
    );
    assert_eq!(
        seen.get(&series("platform_bus_consumer_oldest_unacked_age_seconds")),
        Some(0.0),
        "a caught-up consumer reports zero"
    );
}

#[tokio::test]
async fn the_stream_gauges_match_the_stream_info() {
    let broker = broker().await;
    publish(&broker, 3).await;
    let seen = recorder::Recorded::default();
    let sink = seen.recorder();
    let _installed = metrics::set_default_local_recorder(&sink);
    let mut stream = broker.stream.clone();
    let info = stream.info().await.expect("stream info").clone();
    let now = Timestamp::from_nanosecond(info.state.first_timestamp.unix_timestamp_nanos())
        .expect("an instant")
        .checked_add(jiff::SignedDuration::from_secs(90))
        .expect("an instant");

    sample_bus(&broker.context, &[], now).await;

    #[expect(
        clippy::cast_precision_loss,
        reason = "small counts compared as gauge values"
    )]
    let (messages, bytes, max_bytes) = (
        info.state.messages as f64,
        info.state.bytes as f64,
        info.config.max_bytes as f64,
    );
    assert_eq!(
        seen.get(&stream_series("platform_bus_stream_messages")),
        Some(messages)
    );
    assert_eq!(
        seen.get(&stream_series("platform_bus_stream_bytes")),
        Some(bytes)
    );
    assert_eq!(
        seen.get(&stream_series("platform_bus_stream_max_bytes")),
        Some(max_bytes)
    );
    let age = seen
        .get(&stream_series(
            "platform_bus_stream_oldest_message_age_seconds",
        ))
        .expect("an age");
    assert!(
        (age - 90.0).abs() < 0.001,
        "the oldest message is 90 seconds old: {age}"
    );
    // The command stream is sampled too.
    assert_eq!(
        seen.get("platform_bus_stream_messages{stream=ratatoskr_commands}"),
        Some(0.0)
    );
}

#[tokio::test]
async fn a_durable_that_does_not_exist_is_skipped() {
    let broker = broker().await;
    publish(&broker, 1).await;
    let seen = recorder::Recorded::default();
    let sink = seen.recorder();
    let _installed = metrics::set_default_local_recorder(&sink);

    sample_bus(&broker.context, &[spec()], Timestamp::UNIX_EPOCH).await;

    assert_eq!(
        seen.get(&series("platform_bus_consumer_pending")),
        None,
        "nothing is seen for a durable the broker does not hold"
    );
    assert!(
        seen.get(&stream_series("platform_bus_stream_messages"))
            .is_some(),
        "the stream gauges are still published"
    );
}
