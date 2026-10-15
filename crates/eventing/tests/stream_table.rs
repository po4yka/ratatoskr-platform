//! The fixed durables of the notes write path (workspace change `add-notes-write-back`, spec
//! `fleet-bus-topology`): the durable through which notes reads the write command, which Edge
//! provisions with the domain table, and the durable through which Edge's own consumer projects the
//! conflict events onto the write operations. And the four durables of the sites service (workspace
//! change `add-site-publications`).

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

/// Workspace SITE-025 task 6.12 (spec `fleet-bus-topology`, "The sites durables are provisioned").
#[test]
fn sites_durables_are_provisioned() {
    let row = |stream, durable, filter, ack_wait_seconds| FixedConsumerSpec {
        stream,
        durable_name: durable,
        filter_subject: filter,
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
