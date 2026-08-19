//! The pump: outbox rows onto a real NATS `JetStream`, and the failure paths that no real broker can
//! be made to take on demand.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use platform_eventing::publisher::PublishError;
use platform_eventing::{
    MessageClass, NatsPublisher, Outbox, Publisher, StreamSpec, Subject, pump,
};
use platform_persistence::test_support::TestDatabase;
use uuid::Uuid;

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn subject() -> Subject {
    Subject::new(MessageClass::Command, "content.capture.requested.v1").expect("a valid subject")
}

fn payload(marker: &str) -> serde_json::Value {
    serde_json::json!({ "marker": marker })
}

/// Where the broker is. Matches `compose.yaml`, so `docker compose up -d` then `cargo test` works.
#[expect(
    clippy::disallowed_methods,
    reason = "the workspace bans direct environment reads so configuration has one loader. This is \
              a test binary reading where its broker lives, which is not platform configuration."
)]
fn nats_url() -> String {
    std::env::var("PLATFORM_TEST_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_owned())
}

/// A publisher that fails on command. No real broker can be made to refuse deterministically, and
/// the outbox's backoff and dead-letter behaviour is exactly what must be exercised against refusal.
struct FlakyPublisher {
    failures_remaining: AtomicUsize,
    published: Mutex<Vec<String>>,
}

impl FlakyPublisher {
    fn new(failures: usize) -> Self {
        Self {
            failures_remaining: AtomicUsize::new(failures),
            published: Mutex::new(Vec::new()),
        }
    }
}

impl Publisher for FlakyPublisher {
    async fn publish(
        &self,
        subject: &Subject,
        _payload: &[u8],
        message_id: &str,
    ) -> Result<(), PublishError> {
        if self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(PublishError::NotAcknowledged(Box::new(
                std::io::Error::other("the broker refused"),
            )));
        }
        self.published
            .lock()
            .expect("the lock")
            .push(format!("{subject}/{message_id}"));
        Ok(())
    }
}

/// P-1. A message written to the outbox reaches a real `JetStream`, and the consumer sees it exactly
/// once even though the pump ran twice.
#[tokio::test]
async fn a_message_reaches_jetstream_and_is_not_duplicated() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let publisher = NatsPublisher::connect(&nats_url())
        .await
        .expect("a NATS server; start it with `docker compose up -d`");

    let test_subject = subject();

    // The production stream, not a private one. A wildcard stream covering `cmd.>` may already exist
    // on a shared broker — `ratatoskr-edge` declares one at startup — and JetStream refuses a second
    // stream whose subjects overlap. Using the same declaration the service uses is both what
    // production does and the only thing that works on a broker a service has touched.
    let stream_name = "ratatoskr_commands";
    publisher
        .ensure_stream(&StreamSpec::commands(stream_name, vec!["cmd.>".to_owned()]))
        .await
        .expect("declaring the command stream");
    let stream = publisher
        .context()
        .get_stream(stream_name)
        .await
        .expect("the command stream");
    // Start from empty, so the count below is about this test and not about whatever ran before it.
    stream.purge().await.expect("purging");

    let message_id = Uuid::now_v7();
    Outbox::enqueue(
        pool,
        message_id,
        &test_subject,
        &payload("first"),
        None,
        now(),
    )
    .await
    .expect("enqueuing");

    let report = pump::run_once(pool, &publisher, "publisher", 10, now())
        .await
        .expect("pumping");
    assert_eq!(report.claimed, 1);
    assert_eq!(report.published, 1);
    assert_eq!(report.failed, 0);

    // A second pass finds nothing: the row left the queue.
    let second = pump::run_once(pool, &publisher, "publisher", 10, now())
        .await
        .expect("pumping again");
    assert_eq!(second.claimed, 0);

    let info = publisher
        .context()
        .get_stream(stream_name)
        .await
        .expect("reading the stream")
        .info()
        .await
        .expect("stream info")
        .state
        .messages;
    assert_eq!(info, 1, "the bus must hold exactly one message");

    // The stream is shared and declared by the service, so it is purged rather than deleted:
    // deleting it would break a concurrently running test and any local edge process.
    stream.purge().await.expect("purging");
    harness.cleanup().await.expect("cleanup");
}

/// P-2. A refused publication backs the message off and leaves it in the queue; when the broker
/// recovers, the same message is delivered. Nothing is lost and nothing is duplicated in the outbox.
#[tokio::test]
async fn a_refused_publication_is_retried_until_it_succeeds() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let publisher = FlakyPublisher::new(2);

    let message_id = Uuid::now_v7();
    Outbox::enqueue(pool, message_id, &subject(), &payload("retry"), None, now())
        .await
        .expect("enqueuing");

    // Two passes fail. Each must move the message forward in time, not lose it.
    for attempt in 0..2 {
        let at = now() + jiff::SignedDuration::from_secs(3600 * (attempt + 1));
        let report = pump::run_once(pool, &publisher, "publisher", 10, at)
            .await
            .expect("pumping");
        assert_eq!(report.claimed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.published, 0);
        assert_eq!(report.dead_lettered, 0);
    }

    let at = now() + jiff::SignedDuration::from_secs(3600 * 3);
    let report = pump::run_once(pool, &publisher, "publisher", 10, at)
        .await
        .expect("pumping");
    assert_eq!(
        report.published, 1,
        "the message must survive to be delivered"
    );

    // Cloned out of the guard: holding a `MutexGuard` across the awaits below would make this test
    // a deadlock waiting for a scheduler change.
    let delivered = publisher.published.lock().expect("the lock").clone();
    assert_eq!(
        delivered.len(),
        1,
        "it must be delivered once, not once per attempt"
    );
    assert!(delivered[0].ends_with(&message_id.to_string()));

    let stats = Outbox::stats(pool, at).await.expect("stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.dead_lettered, 0);

    harness.cleanup().await.expect("cleanup");
}

/// P-3. One poison message does not stall the queue: the rest of the batch is still attempted.
#[tokio::test]
async fn one_failing_message_does_not_stop_the_batch() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    // Fails once, then succeeds. The first claimed row takes the failure; the rest must still go.
    let publisher = FlakyPublisher::new(1);

    for index in 0..5 {
        Outbox::enqueue(
            pool,
            Uuid::now_v7(),
            &subject(),
            &payload(&format!("m{index}")),
            None,
            now(),
        )
        .await
        .expect("enqueuing");
    }

    let report = pump::run_once(pool, &publisher, "publisher", 10, now())
        .await
        .expect("pumping");
    assert_eq!(report.claimed, 5);
    assert_eq!(report.failed, 1);
    assert_eq!(
        report.published, 4,
        "the four healthy messages must be delivered despite the first failing"
    );

    harness.cleanup().await.expect("cleanup");
}
