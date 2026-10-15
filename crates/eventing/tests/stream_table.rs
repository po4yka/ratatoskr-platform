//! The fixed durables of the notes write path (workspace change `add-notes-write-back`, spec
//! `fleet-bus-topology`): the durable through which notes reads the write command, which Edge
//! provisions with the domain table, and the durable through which Edge's own consumer projects the
//! conflict events onto the write operations.

use platform_eventing::{
    AckMode, COMMAND_STREAM, DOMAIN_CONSUMERS, EDGE_INTERNAL_CONSUMERS, EVENT_STREAM,
    FixedConsumerSpec, StartAt,
};

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
        DOMAIN_CONSUMERS.len(),
        26,
        "25 rows after OBS-023, plus ratatoskr_notes_write_requests"
    );
    assert_eq!(
        named(&DOMAIN_CONSUMERS, "ratatoskr_notes_write_requests"),
        Some(FixedConsumerSpec {
            stream: COMMAND_STREAM,
            durable_name: "ratatoskr_notes_write_requests",
            filter_subject: "cmd.notes.file.write_requested.v1",
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
            ack: AckMode::Explicit,
            start: StartAt::All,
            ack_wait_seconds: 30,
            max_deliver: -1,
        }),
        "Edge's own consumers create the notes conflict projection"
    );
}
