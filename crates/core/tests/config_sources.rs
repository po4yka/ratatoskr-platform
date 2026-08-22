//! Configuration sources and precedence — tests C-1 … C-6.
//!
//! No test mutates the process environment directly: `std::env::set_var` is `unsafe` in edition
//! 2024 and the workspace forbids unsafe code. `figment::Jail` is the supported route.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]
#![allow(
    clippy::result_large_err,
    reason = "a figment::Jail closure returns figment::Error; the size is figment's, not ours"
)]

use std::collections::BTreeSet;
use std::net::SocketAddr;

use figment::Jail;
use platform_core::RuntimeRole;
use platform_core::config::{self, ConfigError, LogFormat, PlatformConfig};

/// The variables `.env.example` documents, and the probe value each one is given by C-6.
///
/// Milestone 9 grew this from eleven to twenty-four — eighteen when the database, bus and identity
/// keys were finally documented, four more with the retention windows, and two with the request
/// limits `ARCHITECTURE.md` S14 has required since milestone 1. `.env.example` claims to document EVERY
/// configuration variable, and milestones 5 to 8 added the database, the bus and the identity keys
/// without touching it — so the claim had been false for four milestones, and this list is what
/// makes it checkable again.
const DOCUMENTED: [(&str, &str); 25] = [
    ("RATATOSKR__ADMIN__BIND", "127.0.0.1:19464"),
    ("RATATOSKR__PUBLIC__BIND", "127.0.0.1:18080"),
    ("RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS", "17"),
    ("RATATOSKR__PUBLIC__MAX_BODY_BYTES", "2048"),
    ("RATATOSKR__PUBLIC__MAX_CONCURRENT_REQUESTS", "23"),
    ("RATATOSKR__PUBLIC__ACTOR_REQUESTS_PER_MINUTE", "29"),
    (
        "RATATOSKR__DATABASE__URL",
        "postgres://probe:probe@127.0.0.1:15432/probe",
    ),
    ("RATATOSKR__DATABASE__MAX_CONNECTIONS", "3"),
    ("RATATOSKR__DATABASE__ACQUIRE_TIMEOUT_SECONDS", "4"),
    ("RATATOSKR__BUS__URL", "nats://127.0.0.1:14222"),
    // Replaced inside the jail by a file that exists: rule V16 refuses a path that names nothing,
    // and no fixed absolute path is a file on every machine this suite runs on.
    ("RATATOSKR__BUS__NKEY_SEED_PATH", NKEY_SEED_PLACEHOLDER),
    (
        "RATATOSKR__IDENTITY__ASSERTION_KEY",
        // 32 zero bytes. Rule V14 checks the decoded LENGTH and nothing else, which is the whole
        // point of it: a key that is truncated, hex-encoded or accidentally private fails here
        // rather than as an authentication failure that names nothing.
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    ),
    (
        "RATATOSKR__IDENTITY__OAUTH_COMPLETION_URL",
        "https://ratatoskr.example/oauth/done",
    ),
    ("RATATOSKR__RETENTION__INBOX_DAYS", "31"),
    ("RATATOSKR__RETENTION__OUTBOX_DAYS", "32"),
    ("RATATOSKR__RETENTION__AUDIT_DAYS", "33"),
    ("RATATOSKR__RETENTION__SCHEDULE_OCCURRENCE_DAYS", "34"),
    ("RATATOSKR__OPERATIONS__STALE_AFTER_SECONDS", "86400"),
    ("RATATOSKR__SHUTDOWN__DRAIN_SECONDS", "7"),
    ("RATATOSKR__SHUTDOWN__GRACE_SECONDS", "11"),
    ("RATATOSKR__TELEMETRY__LOG_FORMAT", "pretty"),
    ("RATATOSKR__TELEMETRY__LOG_FILTER", "warn"),
    (
        "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
        "https://collector.example:4317",
    ),
    ("RATATOSKR__TELEMETRY__OTLP__TIMEOUT_SECONDS", "9"),
    (
        "RATATOSKR__TELEMETRY__OTLP__HEADERS__AUTHORIZATION",
        "probe-value",
    ),
];

/// What C-6 puts in the nkey variable before it has a real file to point at.
const NKEY_SEED_PLACEHOLDER: &str = "<replaced by the jail>";

/// `.env.example`, read from the repository root.
fn env_example() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env.example");
    std::fs::read_to_string(&path).expect(".env.example must exist at the repository root")
}

/// C-1. Every role's built-in defaults are complete for the operator surface, which is what makes
/// the local-run commands in `DEVELOPMENT.md` true.
///
/// "Complete" is not "sufficient to start". `Ingest` carries no public bind on purpose — a default
/// is a promise the port is free, and on the deployment target `8081` is held by another process —
/// so it loads its defaults and is then refused by rule V1 until an operator names one. That is
/// asserted here rather than left to the role that happens to notice.
#[test]
fn defaults_alone_produce_a_valid_config_for_every_role() {
    Jail::expect_with(|_| {
        for role in RuntimeRole::ALL {
            let defaults = PlatformConfig::defaults(role);

            assert_eq!(
                defaults.admin.bind.port(),
                role.default_admin_port(),
                "{role} must default to its own admin port"
            );
            assert!(
                defaults.admin.bind.ip().is_loopback(),
                "{role} must default to a loopback admin listener"
            );
            assert_eq!(
                defaults.public.is_some(),
                role.default_public_port().is_some(),
                "the public table must be present exactly for the role that defaults to one"
            );

            // Loading applies the rules on top of the defaults. Only the role with no public
            // default is refused, and it is refused for that reason and no other.
            match config::load(role) {
                Ok(config) => assert_eq!(
                    config.admin.bind, defaults.admin.bind,
                    "{role} must load its own admin default"
                ),
                Err(ConfigError::Invalid(found)) => {
                    assert_eq!(
                        role,
                        RuntimeRole::Ingest,
                        "{role} must load on defaults alone"
                    );
                    assert_eq!(found.len(), 1, "{found:?}");
                    assert_eq!(found[0].key, "public.bind");
                }
                Err(other) => panic!("{role}: expected a semantic failure, got {other}"),
            }
        }
        Ok(())
    });
}

/// C-2. The documented precedence rule: the environment wins over the built-in defaults.
#[test]
fn environment_overrides_defaults() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__ADMIN__BIND", "127.0.0.1:19999");
        jail.set_env("RATATOSKR__TELEMETRY__LOG_FORMAT", "pretty");
        jail.set_env("RATATOSKR__SHUTDOWN__DRAIN_SECONDS", "0");

        let config = config::load(RuntimeRole::Edge).expect("the overrides are all valid");

        assert_eq!(
            config.admin.bind,
            "127.0.0.1:19999".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(config.telemetry.log_format, LogFormat::Pretty);
        assert_eq!(config.shutdown.drain_seconds, 0);
        Ok(())
    });
}

/// C-3. Setting one member of a nested table must not drop that table's other defaults.
///
/// Without the serde defaults on `request_timeout_seconds` and `max_body_bytes` this fails inside
/// extraction with `MissingField` — the defect recorded in the specification's verification log.
#[test]
fn a_partially_overridden_nested_table_keeps_its_sibling_defaults() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__PUBLIC__BIND", "127.0.0.1:18080");

        let config = config::load(RuntimeRole::Edge).expect("bind alone must be enough");
        let public = config.public.expect("edge keeps its public table");

        assert_eq!(public.bind.port(), 18080);
        assert_eq!(
            public.request_timeout_seconds, 15,
            "the default must survive"
        );
        assert_eq!(public.max_body_bytes, 1_048_576, "the default must survive");
        Ok(())
    });
}

/// C-4. A typo in a `ConfigMap` is a red pod, not a silent default, and the report names the key.
#[test]
fn an_unknown_environment_key_is_rejected_by_name() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__ADMN__BIND", "127.0.0.1:9464");

        let error = config::load(RuntimeRole::Edge).expect_err("an unknown key must be fatal");
        assert!(
            matches!(error, config::ConfigError::Source(_)),
            "an unknown key is an extraction failure, not a semantic one"
        );

        let report = error.report(RuntimeRole::Edge);
        assert!(
            report.contains("admn"),
            "the report must name the key: {report}"
        );
        Ok(())
    });
}

/// C-5. Only the `RATATOSKR__` prefix is configuration; a bare `RATATOSKR_` variable is not
/// consumed, so the build-identity variables cannot collide with a configuration key.
#[test]
fn the_config_file_path_variable_scheme_has_no_reserved_collisions() {
    Jail::expect_with(|jail| {
        jail.set_env(
            "RATATOSKR_GIT_SHA",
            "0000000000000000000000000000000000000000",
        );
        jail.set_env("RATATOSKR_CONFIG", "/etc/ratatoskr/does-not-exist.toml");
        jail.set_env("RATATOSKR", "ignored");

        config::load(RuntimeRole::Edge).expect("a bare RATATOSKR_ variable is not configuration");
        Ok(())
    });
}

/// C-6. `.env.example` is executable documentation: every variable it names maps to a real field
/// and overrides it, and the file carries no credential.
#[test]
fn every_variable_in_env_example_overrides_its_field() {
    let text = env_example();

    let documented: BTreeSet<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .filter(|line| line.starts_with("RATATOSKR__"))
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name)
        .collect();
    let expected: BTreeSet<&str> = DOCUMENTED.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        documented, expected,
        ".env.example and this test are the same list; if they drift, one of them is wrong"
    );

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#RATATOSKR__TELEMETRY__OTLP__HEADERS__") {
            let value = rest.split_once('=').map_or("", |(_, value)| value);
            assert!(value.is_empty(), "no credential may appear in .env.example");
        }
    }

    Jail::expect_with(|jail| {
        // A real file, because rule V16 refuses a path that names nothing — which is exactly what
        // makes `check-config` useful as a systemd ExecStartPre.
        jail.create_file("edge.nkey", "SUAOCTHSSJR6T33K5ANSWQZ73VAX4LPTXSMYVOXSWEZ")?;
        let seed = jail.directory().join("edge.nkey");

        for (name, value) in DOCUMENTED {
            if value == NKEY_SEED_PLACEHOLDER {
                jail.set_env(name, seed.display());
            } else {
                jail.set_env(name, value);
            }
        }

        let config = config::load(RuntimeRole::Edge).expect("every documented value is valid");
        let public = config.public.expect("edge has a public listener");
        let database = config.database.expect("the database URL was supplied");
        let bus = config.bus.expect("the bus URL was supplied");
        let otlp = config.telemetry.otlp.expect("the endpoint was supplied");

        assert_eq!(config.admin.bind.port(), 19464);
        assert_eq!(public.bind.port(), 18080);
        assert_eq!(public.request_timeout_seconds, 17);
        assert_eq!(public.max_body_bytes, 2048);
        assert_eq!(public.max_concurrent_requests, 23);
        assert_eq!(public.actor_requests_per_minute, 29);
        assert_eq!(database.max_connections, 3);
        assert_eq!(database.acquire_timeout_seconds, 4);
        assert_eq!(bus.url.as_str(), "nats://127.0.0.1:14222");
        assert_eq!(bus.nkey_seed_path.as_deref(), Some(seed.as_path()));
        assert_eq!(
            config.identity.assertion_key.as_deref(),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        );
        assert_eq!(
            config
                .identity
                .oauth_completion_url
                .map(String::from)
                .as_deref(),
            Some("https://ratatoskr.example/oauth/done"),
        );
        assert_eq!(config.retention.inbox_days, 31);
        assert_eq!(config.retention.outbox_days, 32);
        assert_eq!(config.retention.audit_days, 33);
        assert_eq!(config.retention.schedule_occurrence_days, 34);
        assert_eq!(config.shutdown.drain_seconds, 7);
        assert_eq!(config.shutdown.grace_seconds, 11);
        assert_eq!(config.telemetry.log_format, LogFormat::Pretty);
        assert_eq!(config.telemetry.log_filter, "warn");
        assert_eq!(otlp.endpoint.as_str(), "https://collector.example:4317/");
        assert_eq!(otlp.timeout_seconds, 9);
        assert_eq!(
            otlp.headers.keys().collect::<Vec<_>>(),
            vec!["authorization"],
            "a header variable becomes a lowercase header name"
        );
        Ok(())
    });
}

/// A defaults figment is the seam every validation test builds on; proving it here keeps the
/// role-aware defaults honest for the tests in `config_validation.rs`.
#[test]
fn role_defaults_are_the_only_place_a_default_value_is_written() {
    let edge = PlatformConfig::defaults(RuntimeRole::Edge);
    let scheduler = PlatformConfig::defaults(RuntimeRole::Scheduler);

    assert!(edge.public.is_some());
    assert!(scheduler.public.is_none());
    assert_eq!(edge.shutdown.drain_seconds, 5);
    assert_eq!(edge.shutdown.grace_seconds, 25);
    assert_eq!(
        edge.telemetry.log_filter,
        "info,tower_http=info,hyper=warn,h2=warn"
    );
    assert!(edge.telemetry.otlp.is_none());
}
