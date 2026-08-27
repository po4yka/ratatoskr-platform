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

/// The permission stanza for one `NKey` identity, delimited by its public-key placeholder.
fn nkey_stanza<'a>(config: &'a str, identity: &str) -> &'a str {
    let start = config
        .find(identity)
        .unwrap_or_else(|| panic!("missing {identity} identity"));
    let after = config
        .get(start..)
        .expect("a byte offset returned by str::find must be a UTF-8 boundary");
    let end = after
        .find("\n        {\n            nkey:")
        .unwrap_or(after.len());
    after
        .get(..end)
        .expect("a byte offset returned by str::find must be a UTF-8 boundary")
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

/// Every social owner has a distinct, least-privilege NATS identity rather than sharing Edge's
/// broad command publishing credential.
#[test]
fn social_owner_bus_identities_are_limited_to_their_capture_subjects() {
    let raw = deploy("nats/ratatoskr.conf");
    let config: String = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let readme = deploy("nats/README.md");

    for (identity, subject) in [
        ("RATATOSKR_X", "cmd.x.capture.requested.v1"),
        ("RATATOSKR_INSTAGRAM", "cmd.instagram.capture.requested.v1"),
        ("RATATOSKR_THREADS", "cmd.threads.capture.requested.v1"),
    ] {
        let stanza = nkey_stanza(&config, identity);
        assert!(
            readme.contains(subject),
            "{identity} consumer route {subject} is undocumented"
        );
        assert!(
            !stanza.contains("$JS.API.>"),
            "{identity} must not create an arbitrary filtered consumer"
        );
        let durable = match identity {
            "RATATOSKR_X" => "ratatoskr_x_browser_capture",
            "RATATOSKR_INSTAGRAM" => "ratatoskr_instagram_browser_capture",
            "RATATOSKR_THREADS" => "threads_browser_capture",
            _ => unreachable!("the table above is closed"),
        };
        for permission in [
            format!("$JS.API.CONSUMER.INFO.ratatoskr_commands.{durable}"),
            format!("$JS.API.CONSUMER.MSG.NEXT.ratatoskr_commands.{durable}"),
            format!("$JS.ACK.ratatoskr_commands.{durable}.>"),
        ] {
            assert!(
                stanza.contains(&permission),
                "{identity} lacks required permission {permission}"
            );
        }
    }
    for subject in [
        "evt.platform.operation.reported.v1",
        "evt.social.source.captured.v1",
        "evt.social.source.updated.v1",
    ] {
        assert!(
            config.contains(subject),
            "Threads outbox cannot publish {subject}"
        );
    }
}

/// D-7. Domain services have only their workspace-allocated loopback listeners; Edge is the sole
/// public composition point and its template names every prefix/port/class explicitly.
#[test]
fn edge_profile_declares_the_canonical_domain_gateway_table() {
    let text = environment(RuntimeRole::Edge);
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
    }
}
