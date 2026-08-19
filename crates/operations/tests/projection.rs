//! Applying inbound progress events, including once through a real `JetStream`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_eventing::inbox::Outcome;
use platform_eventing::{Incoming, MessageClass, StreamSpec, Subject, deliver};
use platform_operations::ProgressProjection;
use platform_persistence::test_support::TestDatabase;
use ratatoskr_operation_contracts::OperationStatus;
use uuid::Uuid;

const CORRELATION: &str = "correlation:01a0153f-63e5-7010-a4c9-1fe6c43bcc39";

/// Where the broker is. Matches `compose.yaml`.
#[expect(
    clippy::disallowed_methods,
    reason = "the workspace bans direct environment reads so configuration has one loader. This is \
              a test binary choosing which broker to talk to."
)]
fn nats_url() -> String {
    std::env::var("PLATFORM_TEST_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_owned())
}

fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn event(operation_id: Uuid, status: &str) -> Incoming {
    Incoming {
        message_id: Uuid::now_v7(),
        subject: Subject::new(MessageClass::Event, "platform.operation.progressed.v1")
            .expect("a subject"),
        producer: "ratatoskr-extractor".to_owned(),
        payload: serde_json::json!({
            "event_id": Uuid::now_v7(),
            "producer": "ratatoskr-extractor",
            "payload": {
                "operation_id": operation_id,
                "status": status,
                "stage": "downloading",
                "progress_percent": 40,
                "message": "fetching",
            }
        }),
    }
}

/// P-1. An event advances the projection, a redelivery of it changes nothing, and an older status
/// arriving late is absorbed rather than treated as a failure.
#[tokio::test]
async fn the_projection_applies_advances_and_absorbs_redelivery() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = platform_operations::accept(
        pool,
        Uuid::now_v7(),
        "content.capture.submit",
        CORRELATION,
        None,
        now(),
    )
    .await
    .expect("accepting");

    let running = event(operation.operation_id, "running");
    assert_eq!(
        deliver(pool, &ProgressProjection, &running, now())
            .await
            .expect("delivering"),
        Some(Outcome::Applied)
    );
    assert_eq!(
        platform_operations::find(pool, operation.operation_id)
            .await
            .expect("reading")
            .expect("the operation")
            .status,
        OperationStatus::Running
    );

    // The SAME message again: the inbox absorbs it before the handler runs.
    assert_eq!(
        deliver(pool, &ProgressProjection, &running, now())
            .await
            .expect("delivering"),
        None,
        "a redelivered message must not reach the handler"
    );

    // A DIFFERENT message carrying an older status: ordinary traffic under at-least-once delivery.
    let stale = event(operation.operation_id, "queued");
    assert_eq!(
        deliver(pool, &ProgressProjection, &stale, now())
            .await
            .expect("delivering"),
        Some(Outcome::Stale)
    );
    assert_eq!(
        platform_operations::find(pool, operation.operation_id)
            .await
            .expect("reading")
            .expect("the operation")
            .status,
        OperationStatus::Running,
        "a stale event must not move the projection backward"
    );

    harness.cleanup().await.expect("cleanup");
}

/// P-2. An event this build cannot act on is recorded rather than retried forever.
#[tokio::test]
async fn an_unreadable_event_is_recorded_as_rejected() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();

    let mut unknown_operation = event(Uuid::now_v7(), "running");
    assert_eq!(
        deliver(pool, &ProgressProjection, &unknown_operation, now())
            .await
            .expect("delivering"),
        Some(Outcome::Rejected),
        "an event for an operation this platform does not have is not a failure"
    );

    // A NEW message id: reusing the previous one would be a redelivery, and the inbox would absorb
    // it before the handler ever saw the malformed body.
    unknown_operation.message_id = Uuid::now_v7();
    unknown_operation.payload = serde_json::json!({ "event_id": Uuid::now_v7() });
    assert_eq!(
        deliver(pool, &ProgressProjection, &unknown_operation, now())
            .await
            .expect("delivering"),
        Some(Outcome::Rejected)
    );

    harness.cleanup().await.expect("cleanup");
}

/// P-3. The whole loop, through a real broker: a domain service publishes, the consumer applies, and
/// the projection moves. This is the only test that proves the parts are connected.
#[tokio::test]
async fn an_event_published_to_jetstream_reaches_the_projection() {
    let harness = TestDatabase::create().await.expect("a test database");
    let pool = harness.pool();
    let operation = platform_operations::accept(
        pool,
        Uuid::now_v7(),
        "content.capture.submit",
        CORRELATION,
        None,
        now(),
    )
    .await
    .expect("accepting");

    let url = nats_url();
    let publisher = platform_eventing::NatsPublisher::connect(&url)
        .await
        .expect("a NATS server; start it with `docker compose up -d`");

    // The production stream, not a private one: `ratatoskr-edge` declares `evt.>` at startup, and
    // JetStream refuses a second stream whose subjects overlap. The DURABLE CONSUMER is per-test,
    // which is what actually isolates one test's deliveries from another's.
    let stream_name = "ratatoskr_events";
    let consumer_name = format!("c_{}", Uuid::now_v7().simple());
    let subject =
        Subject::new(MessageClass::Event, "platform.operation.progressed.v1").expect("a subject");
    // The same spec the service declares. Both declaration sites take one so that a stream cannot
    // be created with one policy by whichever process reached the broker first.
    let spec = StreamSpec::events(stream_name, vec!["evt.>".to_owned()]);

    publisher
        .ensure_stream(&spec)
        .await
        .expect("declaring the event stream");
    // Start from empty. The stream is shared and retains what earlier runs published; a durable
    // consumer created now would replay all of it, and the count below is meant to be about this
    // test.
    publisher
        .context()
        .get_stream(stream_name)
        .await
        .expect("the event stream")
        .purge()
        .await
        .expect("purging");

    let message = event(operation.operation_id, "succeeded");
    let body = serde_json::to_vec(&message.payload).expect("a body");
    platform_eventing::Publisher::publish(
        &publisher,
        &subject,
        &body,
        &message.message_id.to_string(),
    )
    .await
    .expect("publishing");

    // Run the consumer until it has applied one message, then stop it.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = {
        let pool = pool.clone();
        let context = publisher.context().clone();
        let spec = spec.clone();
        let consumer_name = consumer_name.clone();
        tokio::spawn(async move {
            platform_eventing::consumer::run(
                &context,
                &spec,
                &consumer_name,
                &pool,
                &ProgressProjection,
                async move {
                    let _ = stop_rx.await;
                },
            )
            .await
        })
    };

    // Wait for the projection to move, then stop the consumer.
    let mut moved = false;
    for _ in 0..100 {
        let current = platform_operations::find(pool, operation.operation_id)
            .await
            .expect("reading")
            .expect("the operation");
        if current.status == OperationStatus::Succeeded {
            moved = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _ = stop_tx.send(());
    let report = handle
        .await
        .expect("the consumer task")
        .expect("the consumer");

    assert!(moved, "the published event must reach the projection");
    assert_eq!(report.applied, 1);
    assert_eq!(report.failed, 0);

    // The stream is shared and declared by the service; the per-test durable consumer is what this
    // test owns, so that is what it removes.
    let _ = publisher
        .context()
        .get_stream(stream_name)
        .await
        .expect("the event stream")
        .delete_consumer(&consumer_name)
        .await;
    harness.cleanup().await.expect("cleanup");
}
