//! The deployment profile agrees with the code — tests D-1 … D-6.
//!
//! `deploy/` is prose and configuration, so nothing in it fails to compile. These are the claims it
//! makes that the binaries would contradict silently: a port, a supervisor timeout, a binary path, a
//! stream name. Each one, wrong, produces a service that starts and is unreachable, or a drain that
//! is killed halfway, or a scrape target that is permanently down — never an error message.
//!
//! It lives beside `ratatoskr-edge` because that is the binary that declares the streams and applies
//! `schema.sql`, so it is the one whose constants the profile has to match. The other two roles are
//! reached through `RuntimeRole`, which every binary shares.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_core::RuntimeRole;
use platform_core::config::SHUTDOWN_CEILING_SECONDS;

mod support;

use support::{CMD, EVT, expected_publish_allow};

/// Read a file from `deploy/`, relative to this crate.
fn deploy(path: &str) -> String {
    let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy")
        .join(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("{} must exist and be readable: {error}", full.display()))
}

/// The value of one `Key=value` line, from the first occurrence.
fn setting(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with(&format!("{key}=")))
        .and_then(|line| line.split_once('='))
        .map(|(_, value)| value.trim().to_owned())
}

/// The unit file of one role.
fn unit(role: RuntimeRole) -> String {
    deploy(&format!("systemd/{}.service", role.binary_name()))
}

/// The environment template of one role.
fn environment(role: RuntimeRole) -> String {
    deploy(&format!("systemd/{}.conf.example", role.as_str()))
}

/// D-1. Every unit's stop timeout EXCEEDS the shutdown ceiling the configuration accepts.
///
/// systemd's default `TimeoutStopSec` is 90 seconds and rule V6 accepts `drain + grace` up to 120,
/// so a unit that leaves the default `SIGKILL`s a healthy process thirty seconds into the drain it
/// was told to perform — which is exactly the case where the drain mattered.
#[test]
fn every_unit_waits_longer_than_the_process_may_take_to_stop() {
    for role in RuntimeRole::ALL {
        let text = unit(role);
        let stated = setting(&text, "TimeoutStopSec")
            .unwrap_or_else(|| panic!("{role} names no TimeoutStopSec"));
        let seconds: u64 = stated
            .trim_end_matches('s')
            .parse()
            .unwrap_or_else(|_| panic!("{role} has an unparsable TimeoutStopSec: {stated}"));
        assert!(
            seconds > SHUTDOWN_CEILING_SECONDS,
            "{role} stops at {seconds}s, which is not longer than the {SHUTDOWN_CEILING_SECONDS}s \
             the configuration accepts",
        );
    }
}

/// D-2. Every unit runs the binary of the role it names, at the path `deploy/README.md` installs to.
#[test]
fn every_unit_starts_the_binary_of_its_role() {
    for role in RuntimeRole::ALL {
        let text = unit(role);
        let expected = format!("/usr/local/bin/{}", role.binary_name());
        assert_eq!(
            setting(&text, "ExecStart").as_deref(),
            Some(expected.as_str()),
            "{role}",
        );
        assert_eq!(
            setting(&text, "ExecStartPre").as_deref(),
            Some(format!("{expected} check-config").as_str()),
            "{role} must validate its configuration before it starts",
        );
    }
}

/// D-3. The operator listener in every environment template is on the role's own port.
///
/// The three ports are distinct so all three roles run on one host with no configuration, and the
/// scrape configuration in `deploy/monitoring/` names the same three. A template that carried the
/// wrong one would produce a service that starts, works, and is never scraped.
#[test]
fn every_environment_template_binds_the_admin_port_of_its_role() {
    for role in RuntimeRole::ALL {
        let text = environment(role);
        let bind = setting(&text, "RATATOSKR__ADMIN__BIND")
            .unwrap_or_else(|| panic!("{role} names no admin bind"));
        assert_eq!(
            bind,
            format!("0.0.0.0:{}", role.default_admin_port()),
            "{role}",
        );
        assert!(
            deploy("monitoring/promscrape.ratatoskr.yml")
                .contains(&format!(":{}", role.default_admin_port())),
            "{role}'s operator port is not a scrape target",
        );
    }
}

/// D-4. The role that may not listen publicly has no public bind in its template, and the two that
/// must have one.
///
/// Rule V1 refuses either mistake at startup, so this is not the only defence — but a template that
/// ships a bind for the scheduler is a template that produces a unit which never starts, and the
/// failure would be discovered on the host rather than here.
#[test]
fn only_the_roles_that_may_listen_publicly_carry_a_public_bind() {
    for role in RuntimeRole::ALL {
        let has_bind = setting(&environment(role), "RATATOSKR__PUBLIC__BIND").is_some();
        assert_eq!(has_bind, role.may_have_public_listener(), "{role}");
    }
}

/// D-5. Only `ratatoskr-edge` carries a bus credential.
///
/// ADR-0013: the other two write commands into `operations.outbox` and publish none of them, so a
/// NATS credential for either of them would be a credential on disk that nothing uses — and the
/// `deploy/nats/ratatoskr.conf` identity list would have to grow to match it.
#[test]
fn only_edge_carries_a_bus_credential() {
    for role in RuntimeRole::ALL {
        let text = environment(role);
        let configured = setting(&text, "RATATOSKR__BUS__URL").is_some()
            || setting(&text, "RATATOSKR__BUS__NKEY_SEED_PATH").is_some();
        assert_eq!(configured, role == RuntimeRole::Edge, "{role}");
    }
}

/// D-6. The bus profile names the streams, subjects and consumer the code declares.
///
/// The constants are in `platform_eventing::stream` precisely so there is one source for them, and
/// this is what stops the copy in `deploy/nats/` from becoming a second one. A renamed stream with
/// an unrenamed permission set is a publish that is never acknowledged, reported by the client as
/// "the message was not acknowledged by the bus" — indistinguishable from the broker being down.
#[test]
fn the_bus_profile_names_the_streams_the_code_declares() {
    let raw = deploy("nats/ratatoskr.conf");
    // Comments are stripped, because this file EXPLAINS its permission set at length and a claim
    // about what it grants must be made about the settings rather than about the prose beside them.
    let config: String = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let readme = deploy("nats/README.md");

    assert!(
        config.contains(&format!("\"{}\"", platform_eventing::COMMAND_SUBJECTS)),
        "the permission set does not allow the subject commands are published to",
    );
    for name in [
        platform_eventing::COMMAND_STREAM,
        platform_eventing::EVENT_STREAM,
        platform_eventing::EDGE_PROJECTION_CONSUMER,
    ] {
        assert!(
            readme.contains(name),
            "the bus profile does not name `{name}`"
        );
    }
    // The event subject has no business anywhere in the permission file: edge publishes commands
    // and receives everything else on its own inbox, so a mention of `evt.>` there is either a
    // publish grant nothing uses or a direct subscription that would let a compromised edge tap the
    // bus.
    assert!(
        !config.contains(platform_eventing::EVENT_SUBJECTS),
        "`{}` appears in the permission set, and no Platform process publishes or subscribes to it \
         directly",
        platform_eventing::EVENT_SUBJECTS,
    );
}

/// D-7. Telegram can only inspect, fetch from, and acknowledge its pre-provisioned notification
/// durable. It cannot create a consumer, publish a domain fact, or subscribe directly to events.
#[test]
fn telegram_bus_identity_is_limited_to_its_notification_durable() {
    let identities = deployed_identities();
    let telegram = identities
        .iter()
        .find(|identity| identity.name == "TELEGRAM")
        .expect("the Telegram identity exists");
    let stream = platform_eventing::EVENT_STREAM;
    let durable = platform_eventing::TELEGRAM_NOTIFICATION_CONSUMER;

    for permission in [
        format!("$JS.API.CONSUMER.INFO.{stream}.{durable}"),
        format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.{durable}"),
        format!("$JS.ACK.{stream}.{durable}.>"),
    ] {
        assert!(
            telegram.publish_allow.contains(&permission),
            "Telegram lacks required permission {permission}"
        );
    }
    assert_eq!(
        telegram.publish_allow.len(),
        3,
        "{:?}",
        telegram.publish_allow
    );
    assert_eq!(telegram.subscribe_allow, ["_INBOX.>"]);
    for forbidden in [
        "$JS.API.>",
        "cmd.>",
        "evt.>",
        platform_eventing::TELEGRAM_NOTIFICATION_SUBJECT,
    ] {
        assert!(
            !telegram
                .publish_allow
                .iter()
                .any(|subject| subject == forbidden),
            "Telegram must not receive broad or direct access through {forbidden}"
        );
    }
}

/// D-8. The operator guide and runtime use one fixed Telegram consumer contract and one seed path.
#[test]
fn telegram_consumer_profile_matches_runtime_constants() {
    let readme = deploy("nats/README.md");
    for value in [
        platform_eventing::EVENT_STREAM,
        platform_eventing::TELEGRAM_NOTIFICATION_CONSUMER,
        platform_eventing::TELEGRAM_NOTIFICATION_SUBJECT,
        "/etc/ratatoskr/telegram.nkey",
    ] {
        assert!(
            readme.contains(value),
            "the NATS operator guide does not name `{value}`"
        );
    }
    // Every fixed durable of every table, so a rename in `stream.rs` fails here instead of leaving
    // the operator guide naming a consumer that no longer exists.
    for spec in platform_eventing::SOCIAL_CAPTURE_CONSUMERS
        .iter()
        .chain(platform_eventing::AI_ARCHIVE_REPORT_CONSUMERS.iter())
        .chain(platform_eventing::DOMAIN_CONSUMERS.iter())
    {
        for value in [spec.durable_name, spec.filter_subject] {
            assert!(
                readme.contains(value),
                "the NATS operator guide does not name `{value}`"
            );
        }
    }
}

/// D-7. Domain services have only their workspace-allocated loopback listeners; Edge is the sole
/// public composition point and its template names every prefix/port/class explicitly.
#[test]
fn edge_profile_declares_the_canonical_domain_gateway_table() {
    let text = environment(RuntimeRole::Edge);
    assert_eq!(
        setting(&text, "RATATOSKR__ARCHIVE_STAGING__ROOT").as_deref(),
        Some("/mnt/nvme/ratatoskr/archive-staging")
    );
    assert!(
        unit(RuntimeRole::Edge).contains("ReadWritePaths=/mnt/nvme/ratatoskr/archive-staging"),
        "the sandbox must permit writes only to the configured staging root"
    );
    let tmpfiles = include_str!("../../../deploy/tmpfiles.d/ratatoskr-platform.conf");
    assert_eq!(
        tmpfiles.trim(),
        "d /mnt/nvme/ratatoskr/archive-staging 0700 ratatoskr-edge ratatoskr-edge -"
    );
    assert_eq!(
        setting(&text, "RATATOSKR__PUBLIC__MAX_BODY_BYTES").as_deref(),
        Some("104857600")
    );
    assert_eq!(
        setting(&text, "RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS").as_deref(),
        Some("300")
    );
    for (service, prefix, listener, class) in [
        ("KNOWLEDGE", "/v1/k", "127.0.0.1:8091", "stream"),
        ("GITHUB", "/v1/gh", "127.0.0.1:8092", "control"),
        ("VAULT", "/v1/vault", "127.0.0.1:8093", "transfer"),
        ("SOCIAL", "/v1/social", "127.0.0.1:8094", "stream"),
        ("AI", "/v1/ai", "127.0.0.1:8095", "stream"),
        ("CHATGPT", "/v1/chatgpt", "127.0.0.1:8096", "transfer"),
        ("CLAUDE", "/v1/claude", "127.0.0.1:8097", "transfer"),
    ] {
        let root = format!("RATATOSKR__GATEWAY__ROUTES__{service}");
        assert_eq!(
            setting(&text, &format!("{root}__PREFIX")).as_deref(),
            Some(prefix)
        );
        assert_eq!(
            setting(&text, &format!("{root}__LISTENER")).as_deref(),
            Some(listener)
        );
        assert_eq!(
            setting(&text, &format!("{root}__CLASS")).as_deref(),
            Some(class)
        );
        if matches!(service, "CHATGPT" | "CLAUDE") {
            assert_eq!(
                setting(&text, &format!("{root}__ARCHIVE_RECEIPT_PATH")).as_deref(),
                Some("/v1/ai-archives/receipt")
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The deployed ACL, statically (XR-021 CONTRACTS.md section S03). The real-broker matrix in
// `crates/eventing/tests/deployed_nats_config.rs` proves the same table on the wire.
// ---------------------------------------------------------------------------------------------

fn deployed_identities() -> Vec<platform_nats_profile::Identity> {
    platform_nats_profile::parse(&deploy("nats/ratatoskr.conf")).expect("the deployed ACL parses")
}

fn sorted(mut list: Vec<String>) -> Vec<String> {
    list.sort();
    list
}

/// S03. Thirteen identities, each with exactly the publish list of the contract, subscribing to
/// replies only, and only EDGE carrying a deny list.
#[test]
fn the_deployed_acl_has_the_thirteen_identities_with_exactly_the_contract_grants() {
    let identities = deployed_identities();
    let mut names: Vec<&str> = identities
        .iter()
        .map(|identity| identity.name.as_str())
        .collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = expected_publish_allow()
        .iter()
        .map(|(name, _)| *name)
        .collect();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the deployed identities are not the thirteen of S03"
    );
    assert_eq!(identities.len(), 13);

    for (name, allow) in expected_publish_allow() {
        let identity = identities
            .iter()
            .find(|identity| identity.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"));
        assert_eq!(
            sorted(identity.publish_allow.clone()),
            allow,
            "{name} publish allow"
        );
        assert_eq!(
            identity.subscribe_allow,
            ["_INBOX.>"],
            "{name} may subscribe to replies only"
        );
        if name == "EDGE" {
            assert_eq!(
                sorted(identity.publish_deny.clone()),
                ["$JS.API.STREAM.DELETE.>", "$JS.API.STREAM.PURGE.>"]
            );
        } else {
            assert!(
                identity.publish_deny.is_empty(),
                "{name} carries a deny block"
            );
        }
    }
}

/// S03 invariants 1 to 3: only EDGE holds the control plane, and no other grant creates a consumer
/// or a stream, reads a message by sequence, or uses a wildcard outside the three allowed shapes.
#[test]
fn only_edge_holds_the_control_plane_and_wildcards_are_confined() {
    for identity in deployed_identities() {
        if identity.name == "EDGE" {
            continue;
        }
        for subject in &identity.publish_allow {
            for forbidden in ["$JS.API.>", "evt.>", "cmd.>", "$JS.ACK.>"] {
                assert_ne!(subject, forbidden, "{} holds {forbidden}", identity.name);
            }
            for api in [
                "CONSUMER.CREATE",
                "CONSUMER.DURABLE.CREATE",
                "STREAM.CREATE",
                "STREAM.UPDATE",
                "STREAM.MSG.GET",
                "DIRECT.GET.ratatoskr_",
            ] {
                assert!(
                    !(subject.contains(api) && subject.contains("ratatoskr_")),
                    "{} may call {api} on a ratatoskr stream through {subject}",
                    identity.name
                );
            }
            let wildcard = subject.contains('>') || subject.contains('*');
            let allowed_shape = (subject.starts_with("$JS.ACK.ratatoskr_")
                && subject.ends_with(".>")
                && subject.matches('>').count() == 1)
                || (subject.starts_with("$JS.API.DIRECT.GET.KV_") && subject.ends_with(".>"))
                || (subject.starts_with("$KV.") && subject.ends_with(".>"));
            assert!(
                !wildcard || allowed_shape,
                "{} has a wildcard outside the three allowed shapes: {subject}",
                identity.name
            );
        }
    }
}

/// The extractor's old stanza held `evt.>` and a `$JS.API.>` fragment, so it could read every
/// tenant's events and create consumers. That is deleted, not narrowed in place.
#[test]
fn the_extractor_identity_has_no_broad_grants() {
    let identities = deployed_identities();
    let extractor = identities
        .iter()
        .find(|identity| identity.name == "EXTRACTOR")
        .expect("the extractor identity exists");
    assert_eq!(
        extractor.publish_allow.len(),
        7,
        "{:?}",
        extractor.publish_allow
    );
    for subject in &extractor.publish_allow {
        assert!(
            !subject.contains("evt.>")
                && !subject.contains("$JS.API.>")
                && !subject.contains("cmd.>"),
            "the extractor holds the broad grant {subject}"
        );
    }
}

/// S03 invariant 4: every fixed durable appears in exactly the stanza of its owner, and every
/// durable of the tables in `stream.rs` is owned by some identity.
#[test]
fn every_fixed_consumer_has_exactly_one_owner_identity() {
    // (owner, stream, durable) for every durable that is not EDGE's own.
    let owners: Vec<(&str, &str, &str)> = vec![
        ("TELEGRAM", EVT, "ratatoskr_telegram_notifications"),
        ("X", CMD, "ratatoskr_x_browser_capture"),
        ("X", EVT, "ratatoskr_x_extractor_reports"),
        ("INSTAGRAM", CMD, "ratatoskr_instagram_browser_capture"),
        ("THREADS", CMD, "threads_browser_capture"),
        ("EXTRACTOR", CMD, "ratatoskr_extractor_capture"),
        ("EXTRACTOR", EVT, "ratatoskr_extractor_render_awaits"),
        ("EXTRACTOR_BROWSER_WORKER", CMD, "ratatoskr_browser_worker"),
        ("KNOWLEDGE", CMD, "ratatoskr_knowledge_channel_recap"),
        ("KNOWLEDGE", EVT, "ratatoskr_knowledge_documents"),
        ("KNOWLEDGE", EVT, "ratatoskr_knowledge_social_sources"),
        ("KNOWLEDGE", EVT, "ratatoskr_knowledge_ai_archive"),
        ("KNOWLEDGE", EVT, "ratatoskr_knowledge_repository_requests"),
        ("GITHUB", EVT, "ratatoskr_github_analysis_completed"),
        ("GITHUB", EVT, "ratatoskr_github_analysis_failed"),
        ("GITHUB", EVT, "ratatoskr_github_policy_acknowledged"),
        ("VAULT", CMD, "ratatoskr_vault_backup_policy"),
        (
            "CHANNEL_DIGESTS",
            CMD,
            "ratatoskr_channel_digest_subscriptions",
        ),
        ("CHANNEL_DIGESTS", CMD, "ratatoskr_channel_digest_runs"),
        (
            "CHANNEL_DIGESTS",
            CMD,
            "ratatoskr_channel_digest_schedule_occurrences",
        ),
        (
            "CHANNEL_DIGESTS",
            EVT,
            "ratatoskr_channel_digest_recap_completed",
        ),
        (
            "CHANNEL_DIGESTS",
            EVT,
            "ratatoskr_channel_digest_recap_failed",
        ),
    ];
    let identities = deployed_identities();
    for (owner, stream, durable) in &owners {
        let info = format!("$JS.API.CONSUMER.INFO.{stream}.{durable}");
        let holders: Vec<&str> = identities
            .iter()
            .filter(|identity| identity.publish_allow.contains(&info))
            .map(|identity| identity.name.as_str())
            .collect();
        assert_eq!(holders, [*owner], "{durable} is held by {holders:?}");
    }

    // The tables in stream.rs: each durable is either EDGE's own (the projections it consumes) or
    // owned above, so a durable added to the code without a stanza fails here.
    let edge_owned = [
        "platform_ai_archive_chatgpt_projection",
        "platform_ai_archive_claude_projection",
    ];
    for spec in platform_eventing::SOCIAL_CAPTURE_CONSUMERS
        .iter()
        .chain(platform_eventing::AI_ARCHIVE_REPORT_CONSUMERS.iter())
        .chain(platform_eventing::DOMAIN_CONSUMERS.iter())
    {
        let known = edge_owned.contains(&spec.durable_name)
            || owners
                .iter()
                .any(|(_, _, durable)| *durable == spec.durable_name);
        assert!(
            known,
            "{} has no owner identity in the ACL",
            spec.durable_name
        );
    }
    assert_eq!(
        platform_eventing::DOMAIN_CONSUMERS.len(),
        18,
        "S04 has seven command durables and eleven event durables"
    );
}

/// The operator guide lists every identity with its seed file and every fixed durable, and states
/// the order in which a change is rolled out.
#[test]
fn the_operator_guide_documents_every_identity_and_the_rollout_order() {
    let readme = deploy("nats/README.md");
    for seed in [
        "edge.nkey",
        "telegram.nkey",
        "chatgpt.nkey",
        "claude.nkey",
        "x.nkey",
        "instagram.nkey",
        "threads.nkey",
        "extractor.nkey",
        "extractor-browser-worker.nkey",
        "knowledge.nkey",
        "github.nkey",
        "vault.nkey",
        "channel-digests.nkey",
    ] {
        assert!(readme.contains(seed), "the NATS guide does not name {seed}");
    }
    for value in [
        "BROWSER_NKEY_SEED_PATH",
        "browser_worker_completions",
        "Publish Violation",
        "reload NATS",
        "restart Edge",
    ] {
        assert!(
            readme.contains(value),
            "the NATS guide does not say `{value}`"
        );
    }
}

/// CI starts the smoke broker from the rendered deployed configuration, so a permission the
/// binaries need and the ACL withholds fails in CI and not on the host.
#[test]
fn ci_smoke_uses_the_rendered_deployed_nats_config() {
    let workflow = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml"),
    )
    .expect("the CI workflow is readable");
    let start = workflow
        .find("name: The artifact serves and stops cleanly")
        .expect("the smoke step exists");
    let step = workflow.get(start..).expect("the step text");
    let step = step.split("\n      - name:").next().unwrap_or(step);

    assert!(
        step.contains(
            "--bin render-nats-config -- deploy/nats/ratatoskr.conf \"$RUNNER_TEMP/nats\""
        ),
        "the smoke step does not render the deployed ACL"
    );
    let broker = step
        .split("docker run -d --name smoke-bus")
        .nth(1)
        .and_then(|rest| rest.split("for _ in").next())
        .expect("the smoke broker is started");
    assert!(
        broker.contains("--user root"),
        "the broker needs the store path"
    );
    assert!(
        broker.contains("-v \"$RUNNER_TEMP/nats:/etc/nats-test:ro\"")
            && broker.contains("-c /etc/nats-test/ratatoskr.conf"),
        "the smoke broker is not started on the rendered configuration:\n{broker}"
    );
    let edge = step
        .split("docker run -d --name smoke --network host")
        .nth(1)
        .expect("edge is started");
    assert!(
        edge.contains("RATATOSKR__BUS__NKEY_SEED_PATH=/etc/nats-test/edge.nkey")
            && edge.contains("/etc/nats-test"),
        "edge is not given the EDGE seed"
    );
}
