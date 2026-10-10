//! The S03 publish allow lists of the NATS ACL (XR-021 CONTRACTS.md), written out as data.
//!
//! The contract is spelled here and not derived from `deploy/nats/ratatoskr.conf`, so a stanza that
//! is widened fails the test that compares the two.

pub(crate) const CMD: &str = "ratatoskr_commands";
pub(crate) const EVT: &str = "ratatoskr_events";

/// `TRIO(stream, durable)` of the contract: inspect, pull, and acknowledge exactly one durable.
fn trio(stream: &str, durable: &str) -> Vec<String> {
    vec![
        format!("$JS.API.CONSUMER.INFO.{stream}.{durable}"),
        format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.{durable}"),
        format!("$JS.ACK.{stream}.{durable}.>"),
    ]
}

/// `PULL2(stream, durable)`: inspect and pull a durable that takes no acknowledgements.
fn pull2(stream: &str, durable: &str) -> Vec<String> {
    vec![
        format!("$JS.API.CONSUMER.INFO.{stream}.{durable}"),
        format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.{durable}"),
    ]
}

fn subjects(list: &[&str]) -> Vec<String> {
    list.iter().map(|subject| (*subject).to_owned()).collect()
}

fn ai_archive(report: &str) -> Vec<String> {
    let mut allow = subjects(&[
        report,
        "evt.ai_archive.archive.imported.v1",
        "evt.ai_archive.conversation.added.v1",
        "evt.ai_archive.conversation.updated.v1",
        "evt.ai_archive.project.added.v1",
        "evt.ai_archive.project.updated.v1",
        "evt.ai_archive.artifact.added.v1",
        "evt.ai_archive.artifact.updated.v1",
        "evt.ai_archive.subject.tombstoned.v1",
    ]);
    allow.sort();
    allow
}

fn social_facts() -> Vec<&'static str> {
    vec![
        "evt.platform.operation.reported.v1",
        "evt.social.source.captured.v1",
        "evt.social.source.updated.v1",
        "evt.social.source.removed.v1",
    ]
}

/// S03, publish allow lists, exact, sorted for comparison. The contract is written out here and not
/// derived from the configuration, so widening a stanza fails this test.
fn expected_platform_and_social() -> Vec<(&'static str, Vec<String>)> {
    let join = |parts: Vec<Vec<String>>| {
        let mut all: Vec<String> = parts.into_iter().flatten().collect();
        all.sort();
        all
    };
    vec![
        (
            "EDGE",
            join(vec![subjects(&["cmd.>", "$JS.API.>", "$JS.ACK.>"])]),
        ),
        (
            "TELEGRAM",
            join(vec![trio(EVT, "ratatoskr_telegram_notifications")]),
        ),
        (
            "CHATGPT",
            ai_archive("evt.ai-archive.chatgpt.operation.reported.v1"),
        ),
        (
            "CLAUDE",
            ai_archive("evt.ai-archive.claude.operation.reported.v1"),
        ),
        (
            "X",
            join(vec![
                trio(CMD, "ratatoskr_x_browser_capture"),
                trio(EVT, "ratatoskr_x_extractor_reports"),
                subjects(&social_facts()),
                subjects(&["cmd.content.capture.requested.v1"]),
            ]),
        ),
        (
            "INSTAGRAM",
            join(vec![
                trio(CMD, "ratatoskr_instagram_browser_capture"),
                subjects(&social_facts()),
            ]),
        ),
        (
            "THREADS",
            join(vec![
                trio(CMD, "threads_browser_capture"),
                subjects(&social_facts()),
            ]),
        ),
    ]
}

/// The extractor, the domain services and the digest worker.
fn expected_domain_services() -> Vec<(&'static str, Vec<String>)> {
    let join = |parts: Vec<Vec<String>>| {
        let mut all: Vec<String> = parts.into_iter().flatten().collect();
        all.sort();
        all
    };
    vec![
        (
            "EXTRACTOR",
            join(vec![
                subjects(&[
                    "evt.content.document.extracted.v1",
                    "evt.platform.operation.reported.v1",
                    "cmd.content.render.requested.v1",
                ]),
                trio(CMD, "ratatoskr_extractor_capture"),
                pull2(EVT, "ratatoskr_extractor_render_awaits"),
            ]),
        ),
        (
            "EXTRACTOR_BROWSER_WORKER",
            join(vec![
                subjects(&[
                    "evt.content.render.completed.v1",
                    "evt.content.render.failed.v1",
                    "$JS.API.STREAM.INFO.KV_browser_worker_completions",
                    "$JS.API.DIRECT.GET.KV_browser_worker_completions.>",
                    "$KV.browser_worker_completions.>",
                ]),
                trio(CMD, "ratatoskr_browser_worker"),
            ]),
        ),
        (
            "KNOWLEDGE",
            join(vec![
                subjects(&[
                    "evt.knowledge.analysis.completed.v1",
                    "evt.knowledge.ai_archive_analysis.completed.v1",
                    "evt.knowledge.repository_analysis.completed.v1",
                    "evt.knowledge.repository_analysis.failed.v1",
                    "evt.knowledge.channel_digest_recap.completed.v1",
                    "evt.knowledge.channel_digest_recap.failed.v1",
                ]),
                trio(CMD, "ratatoskr_knowledge_channel_recap"),
                trio(EVT, "ratatoskr_knowledge_documents"),
                trio(EVT, "ratatoskr_knowledge_social_sources"),
                trio(EVT, "ratatoskr_knowledge_ai_archive"),
                trio(EVT, "ratatoskr_knowledge_repository_requests"),
            ]),
        ),
        (
            "GITHUB",
            join(vec![
                subjects(&[
                    "evt.knowledge.repository_analysis.requested.v1",
                    "cmd.vault.backup_policy.apply_requested.v1",
                ]),
                trio(EVT, "ratatoskr_github_analysis_completed"),
                trio(EVT, "ratatoskr_github_analysis_failed"),
                trio(EVT, "ratatoskr_github_policy_acknowledged"),
            ]),
        ),
        (
            "VAULT",
            join(vec![
                subjects(&["evt.vault.backup_policy.acknowledged.v1"]),
                trio(CMD, "ratatoskr_vault_backup_policy"),
            ]),
        ),
        (
            "CHANNEL_DIGESTS",
            join(vec![
                subjects(&[
                    "cmd.knowledge.channel_digest_recap.requested.v1",
                    "evt.platform.operation.reported.v1",
                    "cmd.platform.schedule.registration_requested.v1",
                ]),
                trio(CMD, "ratatoskr_channel_digest_subscriptions"),
                trio(CMD, "ratatoskr_channel_digest_runs"),
                trio(CMD, "ratatoskr_channel_digest_schedule_occurrences"),
                trio(EVT, "ratatoskr_channel_digest_recap_completed"),
                trio(EVT, "ratatoskr_channel_digest_recap_failed"),
            ]),
        ),
    ]
}

pub(crate) fn expected_publish_allow() -> Vec<(&'static str, Vec<String>)> {
    let mut all = expected_platform_and_social();
    all.extend(expected_domain_services());
    all
}
