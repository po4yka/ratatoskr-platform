//! Startup validation rules V1 … V9 and the operator-facing report — tests C-7 … C-18.
//!
//! `figment::Jail` is the only supported way to set an environment variable here: `std::env::set_var`
//! is `unsafe` in edition 2024 and the workspace forbids unsafe code.

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

use figment::providers::Serialized;
use figment::{Figment, Jail};
use platform_core::RuntimeRole;
use platform_core::config::{self, ConfigError, PlatformConfig, Violation};

/// The violations `load` reports for `role`, or a panic if the configuration was accepted.
fn violations(role: RuntimeRole) -> Vec<Violation> {
    match config::load(role) {
        Ok(config) => panic!("{role} must reject this configuration, got {config:?}"),
        Err(ConfigError::Invalid(found)) => found,
        Err(other) => panic!("expected a semantic failure, got {other}"),
    }
}

/// Whether a violation set names `key`.
fn names(found: &[Violation], key: &str) -> bool {
    found.iter().any(|violation| violation.key == key)
}

/// C-7. `ARCHITECTURE.md` S18: the edge role serves the public API, so a missing public listener is
/// a startup failure and not a quietly headless process.
#[test]
fn edge_requires_a_public_listener() {
    // The scheduler defaults are the edge defaults minus the public table, which is exactly the
    // configuration under test. Milestone 7 gave ingest a listener of its own, so it is no longer
    // the role whose defaults omit one.
    let without_public = Figment::from(Serialized::defaults(PlatformConfig::defaults(
        RuntimeRole::Scheduler,
    )));

    let error = config::load_from(RuntimeRole::Edge, without_public)
        .expect_err("edge without a public listener must not start");
    let ConfigError::Invalid(found) = error else {
        panic!("expected a semantic failure");
    };

    assert!(names(&found, "public.bind"));
    assert!(
        found[0]
            .rule
            .contains("edge role requires a public listener")
    );
}

/// C-8. `ARCHITECTURE.md` S18, enforced at startup rather than documented: a scheduler that opens a
/// public port is a security-boundary violation no other test would catch.
#[test]
fn scheduler_rejects_a_configured_public_listener() {
    Jail::expect_with(|jail| {
        // One variable, and the whole public table must still extract — the serde defaults on the
        // other members are what let rule V1 produce its message instead of a `MissingField`.
        jail.set_env("RATATOSKR__PUBLIC__BIND", "0.0.0.0:8080");

        let found = violations(RuntimeRole::Scheduler);

        assert_eq!(found.len(), 1, "exactly the listener rule fires: {found:?}");
        assert_eq!(found[0].key, "public.bind");
        assert_eq!(found[0].env_var, "RATATOSKR__PUBLIC__BIND");
        assert!(
            found[0]
                .rule
                .contains("scheduler role must not open a public listener")
        );
        Ok(())
    });
}

/// C-9. `ARCHITECTURE.md` S9: the webhook adapter of milestone 7 is reached from outside, so an
/// ingest process without a listener is one no source can deliver to.
///
/// This test asserted the opposite until milestone 7, when the adapter arrived. The change is the
/// one line `RuntimeRole::may_have_public_listener` always said it would be.
#[test]
fn ingest_requires_a_public_listener() {
    let without_public = Figment::from(Serialized::defaults(PlatformConfig::defaults(
        RuntimeRole::Scheduler,
    )));

    let error = config::load_from(RuntimeRole::Ingest, without_public)
        .expect_err("ingest without a public listener must not start");
    let ConfigError::Invalid(found) = error else {
        panic!("expected a semantic failure");
    };

    assert!(names(&found, "public.bind"));
    assert!(
        found[0]
            .rule
            .contains("ingest role requires a public listener"),
        "{found:?}"
    );
}

/// C-9b. Only `Edge` defaults to a public bind. `Ingest` may listen publicly and must, but its port
/// is an allocation rather than a default.
///
/// A default is a promise that the port is free. On the deployment target that promise is false —
/// `8081` is held by another process — so a compiled default buys a crash loop whose error names an
/// address instead of an allocation, and whose reflexive repair is a wildcard bind that publishes
/// the webhook surface to the whole network. The absent default is what makes rule V1 refuse the
/// process until an operator names the bind.
#[test]
fn only_edge_defaults_to_a_public_bind() {
    let edge = PlatformConfig::defaults(RuntimeRole::Edge);
    let ingest = PlatformConfig::defaults(RuntimeRole::Ingest);

    let Some(edge_public) = edge.public else {
        panic!("edge defaults to a public listener");
    };
    assert!(
        ingest.public.is_none(),
        "ingest must not carry a compiled public default"
    );
    assert!(RuntimeRole::Ingest.may_have_public_listener());

    // The admin ports still differ, so all three binaries run on one developer machine.
    assert_ne!(edge.admin.bind, ingest.admin.bind);
    assert_ne!(edge_public.bind, edge.admin.bind);
    assert_ne!(edge_public.bind, ingest.admin.bind);
}

/// C-9c. Ingest on its own defaults refuses to start, and the refusal names the variable.
#[test]
fn ingest_on_its_own_defaults_is_refused() {
    let defaults = Figment::from(Serialized::defaults(PlatformConfig::defaults(
        RuntimeRole::Ingest,
    )));

    let error = config::load_from(RuntimeRole::Ingest, defaults)
        .expect_err("ingest must not start without an explicit public bind");
    let ConfigError::Invalid(found) = error else {
        panic!("expected a semantic failure");
    };
    assert!(names(&found, "public.bind"));
    assert_eq!(found[0].env_var, "RATATOSKR__PUBLIC__BIND");
}

/// C-10. `AGENTS.md` keeps the operator plane off the public surface; sharing one address would
/// publish `/metrics` on the public port through a single typo.
#[test]
fn admin_and_public_must_not_share_a_bind_address() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__ADMIN__BIND", "127.0.0.1:9464");
        jail.set_env("RATATOSKR__PUBLIC__BIND", "127.0.0.1:9464");

        let found = violations(RuntimeRole::Edge);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, "public.bind");
        assert!(found[0].rule.contains("must not equal admin.bind"));
        Ok(())
    });
}

/// C-11. V5: a bad filter otherwise silences every log line at the moment they are needed, and it
/// must fail as a configuration problem on stderr rather than inside subscriber setup.
#[test]
fn an_unparsable_log_filter_is_rejected_at_startup_not_at_init() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__TELEMETRY__LOG_FILTER", "=====nope");

        let found = violations(RuntimeRole::Edge);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, "telemetry.log_filter");
        Ok(())
    });
}

/// C-12. V6: a drain plus grace total above the pod termination grace period guarantees SIGKILL
/// mid-request.
#[test]
fn shutdown_windows_are_bounded_and_their_sum_is_bounded() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__SHUTDOWN__DRAIN_SECONDS", "61");
        assert!(names(
            &violations(RuntimeRole::Edge),
            "shutdown.drain_seconds"
        ));
        Ok(())
    });

    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__SHUTDOWN__GRACE_SECONDS", "0");
        assert!(names(
            &violations(RuntimeRole::Edge),
            "shutdown.grace_seconds"
        ));
        Ok(())
    });

    Jail::expect_with(|jail| {
        // Both members are individually in range; only their sum is not.
        jail.set_env("RATATOSKR__SHUTDOWN__DRAIN_SECONDS", "60");
        jail.set_env("RATATOSKR__SHUTDOWN__GRACE_SECONDS", "100");

        let found = violations(RuntimeRole::Edge);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, "shutdown.grace_seconds");
        assert!(found[0].rule.contains("must not exceed 120"));
        Ok(())
    });

    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__SHUTDOWN__DRAIN_SECONDS", "60");
        jail.set_env("RATATOSKR__SHUTDOWN__GRACE_SECONDS", "60");
        config::load(RuntimeRole::Edge).expect("the boundary value is legal");
        Ok(())
    });
}

/// C-13. V3 and V4: `ARCHITECTURE.md` S5.2 transport timeouts and S14 body limits.
#[test]
fn body_limit_and_timeout_ranges_are_enforced() {
    for (variable, value, key) in [
        (
            "RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS",
            "0",
            "public.request_timeout_seconds",
        ),
        (
            "RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS",
            "301",
            "public.request_timeout_seconds",
        ),
        (
            "RATATOSKR__PUBLIC__MAX_BODY_BYTES",
            "1023",
            "public.max_body_bytes",
        ),
        (
            "RATATOSKR__PUBLIC__MAX_BODY_BYTES",
            "104857601",
            "public.max_body_bytes",
        ),
    ] {
        Jail::expect_with(|jail| {
            jail.set_env(variable, value);
            let found = violations(RuntimeRole::Edge);
            assert!(
                names(&found, key),
                "{variable}={value} must be rejected: {found:?}"
            );
            Ok(())
        });
    }

    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS", "300");
        jail.set_env("RATATOSKR__PUBLIC__MAX_BODY_BYTES", "104857600");
        config::load(RuntimeRole::Edge).expect("the boundary values are legal");
        Ok(())
    });
}

/// C-14. V7 and V9: a header name carrying a control character is a request-splitting primitive,
/// and a non-HTTP endpoint scheme is a misconfiguration the exporter cannot recover from.
#[test]
fn otlp_endpoint_scheme_and_header_name_grammar_are_enforced() {
    Jail::expect_with(|jail| {
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
            "ftp://collector.example",
        );

        let found = violations(RuntimeRole::Edge);
        assert!(names(&found, "telemetry.otlp.endpoint"), "{found:?}");
        Ok(())
    });

    Jail::expect_with(|jail| {
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
            "https://collector.example:4317",
        );
        // `bad_name` is outside ^[a-z0-9-]{1,64}$.
        jail.set_env("RATATOSKR__TELEMETRY__OTLP__HEADERS__BAD_NAME", "value");

        let found = violations(RuntimeRole::Edge);
        assert!(names(&found, "telemetry.otlp.headers"), "{found:?}");
        Ok(())
    });

    Jail::expect_with(|jail| {
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
            "https://collector.example:4317",
        );
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__HEADERS__AUTHORIZATION",
            "value",
        );
        config::load(RuntimeRole::Edge).expect("a lowercase header name is legal");
        Ok(())
    });
}

/// C-14b. V8: the one rule that stops an exporter timeout of `0` or an hour. Every sibling rule has
/// a test; without this one the whole V8 block can be deleted with the suite still green.
#[test]
fn the_otlp_timeout_range_is_enforced() {
    for value in ["0", "61"] {
        Jail::expect_with(|jail| {
            jail.set_env(
                "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
                "https://collector.example:4317",
            );
            jail.set_env("RATATOSKR__TELEMETRY__OTLP__TIMEOUT_SECONDS", value);

            let found = violations(RuntimeRole::Edge);
            assert!(
                names(&found, "telemetry.otlp.timeout_seconds"),
                "timeout_seconds={value} must be rejected: {found:?}"
            );
            Ok(())
        });
    }

    for value in ["1", "60"] {
        Jail::expect_with(|jail| {
            jail.set_env(
                "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
                "https://collector.example:4317",
            );
            jail.set_env("RATATOSKR__TELEMETRY__OTLP__TIMEOUT_SECONDS", value);
            config::load(RuntimeRole::Edge).expect("the boundary value is legal");
            Ok(())
        });
    }
}

/// C-14c. V10: `Url`'s `Debug` prints `username`, `password` and `query` as plain fields and the
/// whole configuration is rendered with `Debug` into the startup line and into `check-config`, so
/// an endpoint is a credential carrier that no `SecretString` covers. `SECURITY.md` "redact
/// secrets"; `AGENTS.md` "never log … secret headers".
#[test]
fn an_otlp_endpoint_may_not_carry_a_credential() {
    for endpoint in [
        "https://otel:LEAKME@collector.example:4317",
        "https://otel@collector.example:4317",
        "https://collector.example:4317/v1/traces?access_token=LEAKME",
    ] {
        Jail::expect_with(|jail| {
            jail.set_env("RATATOSKR__TELEMETRY__OTLP__ENDPOINT", endpoint);

            let found = violations(RuntimeRole::Edge);
            assert!(
                names(&found, "telemetry.otlp.endpoint"),
                "{endpoint} must be rejected: {found:?}"
            );

            let report = ConfigError::Invalid(found).report(RuntimeRole::Edge);
            assert!(!report.contains("LEAKME"), "the report echoed it: {report}");
            Ok(())
        });
    }

    Jail::expect_with(|jail| {
        // A path is not a credential and stays legal: several collectors are reached on one.
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
            "https://collector.example:4317/v1/traces",
        );
        config::load(RuntimeRole::Edge).expect("a path-only endpoint is legal");
        Ok(())
    });
}

/// C-15. figment is fail-fast, so this proves that *our* pass collects every problem: an operator
/// editing a `ConfigMap` gets one round trip and not five.
#[test]
fn validation_reports_every_violation_not_only_the_first() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__PUBLIC__BIND", "0.0.0.0:8080");
        jail.set_env("RATATOSKR__SHUTDOWN__GRACE_SECONDS", "999");

        let error = config::load(RuntimeRole::Scheduler).expect_err("two problems, both fatal");
        let ConfigError::Invalid(found) = &error else {
            panic!("expected a semantic failure");
        };
        assert_eq!(found.len(), 2, "{found:?}");

        assert_eq!(
            error.report(RuntimeRole::Scheduler),
            "\
ratatoskr-scheduler: refusing to start; 2 configuration problems.

  public.bind
      RATATOSKR__PUBLIC__BIND
      the scheduler role must not open a public listener (ARCHITECTURE.md S18)

  shutdown.grace_seconds
      RATATOSKR__SHUTDOWN__GRACE_SECONDS
      must be 1..=120, and drain_seconds + grace_seconds must not exceed 120

Supplied values are never echoed.
Validate without starting: ratatoskr-scheduler check-config
"
        );
        Ok(())
    });
}

/// C-16. `SECURITY.md`: the report is built from `&'static str` only, so no supplied value — and
/// therefore no secret — can reach it, whether extraction or validation failed.
#[test]
fn a_configuration_report_never_contains_a_supplied_value() {
    Jail::expect_with(|jail| {
        // Every variable set to the sentinel: extraction fails on the first one figment reaches.
        for variable in [
            "RATATOSKR__ADMIN__BIND",
            "RATATOSKR__PUBLIC__BIND",
            "RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS",
            "RATATOSKR__PUBLIC__MAX_BODY_BYTES",
            "RATATOSKR__SHUTDOWN__DRAIN_SECONDS",
            "RATATOSKR__SHUTDOWN__GRACE_SECONDS",
            "RATATOSKR__TELEMETRY__LOG_FORMAT",
            "RATATOSKR__TELEMETRY__LOG_FILTER",
            "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
            "RATATOSKR__TELEMETRY__OTLP__TIMEOUT_SECONDS",
            "RATATOSKR__TELEMETRY__OTLP__HEADERS__AUTHORIZATION",
        ] {
            jail.set_env(variable, "LEAKME");
        }

        let error = config::load(RuntimeRole::Edge).expect_err("LEAKME is not a socket address");
        let report = error.report(RuntimeRole::Edge);
        assert!(
            !report.contains("LEAKME"),
            "the report echoed a value: {report}"
        );
        Ok(())
    });

    // figment is fail-fast, so the block above dies on `admin.bind`, whose message never quotes the
    // value. These are the field types whose figment message DOES quote it — `invalid type: found
    // string "LEAKME", expected u64`, `relative URL without a base: "LEAKME"`, ``unknown variant:
    // found `LEAKME` `` — which is the arm the no-echo guarantee exists for. One variable each, so
    // extraction fails on exactly the key under test.
    for variable in [
        "RATATOSKR__PUBLIC__MAX_BODY_BYTES",
        "RATATOSKR__SHUTDOWN__GRACE_SECONDS",
        "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
        "RATATOSKR__TELEMETRY__LOG_FORMAT",
    ] {
        Jail::expect_with(|jail| {
            jail.set_env(variable, "LEAKME");

            let error = config::load(RuntimeRole::Edge)
                .expect_err("LEAKME is not a value of any of these fields");
            let report = error.report(RuntimeRole::Edge);
            assert!(
                !report.contains("LEAKME"),
                "{variable}: the report echoed a value: {report}"
            );
            // The `Display` is never the report, and must not become it: figment's message is what
            // carries the value, and `ConfigError::Source` deliberately does not interpolate it.
            assert!(
                !error.to_string().contains("LEAKME"),
                "{variable}: the Display echoed a value: {error}"
            );
            Ok(())
        });
    }

    Jail::expect_with(|jail| {
        // A missing member nested in a table. figment reports it under the PARENT's path, so the
        // report must append the member's own name: `telemetry.otlp` was supplied and
        // `RATATOSKR__TELEMETRY__OTLP` is not a variable anyone can set.
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__HEADERS__AUTHORIZATION",
            "Bearer LEAKME",
        );

        let error = config::load(RuntimeRole::Edge).expect_err("the endpoint was not supplied");
        let report = error.report(RuntimeRole::Edge);
        assert!(
            !report.contains("LEAKME"),
            "the report echoed a value: {report}"
        );
        assert!(
            report.contains("telemetry.otlp.endpoint")
                && report.contains("RATATOSKR__TELEMETRY__OTLP__ENDPOINT"),
            "the report must name a key and a variable the operator can act on: {report}"
        );
        Ok(())
    });

    Jail::expect_with(|jail| {
        // A configuration that extracts but violates a rule, with the sentinel inside the value.
        jail.set_env("RATATOSKR__TELEMETRY__LOG_FILTER", "=====LEAKME");

        let error = config::load(RuntimeRole::Edge).expect_err("the filter does not parse");
        let report = error.report(RuntimeRole::Edge);
        assert!(
            !report.contains("LEAKME"),
            "the report echoed a value: {report}"
        );
        assert!(report.contains("telemetry.log_filter"));
        Ok(())
    });
}

/// C-17. `AGENTS.md`, "never log a secret header": the effective-configuration line renders the
/// configuration with `Debug`, and the type makes that safe without a redactor.
///
/// The startup line itself is covered by construction rather than by capture: `announce` emits
/// `config = ?config` and nothing else derived from the configuration, so the `Debug` rendering
/// asserted here IS the text of that line. The mechanism, not the transcript, is what this pins —
/// and rule V10 closes the one member of the tree whose `Debug` was not safe.
#[test]
fn a_secret_never_appears_in_debug_output_or_in_the_effective_config_log() {
    Jail::expect_with(|jail| {
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
            "https://collector.example:4317",
        );
        jail.set_env(
            "RATATOSKR__TELEMETRY__OTLP__HEADERS__AUTHORIZATION",
            "Bearer canary-do-not-log",
        );

        let config = config::load(RuntimeRole::Edge).expect("a valid exporter configuration");

        let debug = format!("{config:?}");
        assert!(
            !debug.contains("canary"),
            "Debug leaked the secret: {debug}"
        );
        assert!(
            debug.contains("REDACTED"),
            "the secret must render redacted: {debug}"
        );

        // The startup line logs `config = ?cfg`; serialization is the other way a value escapes.
        let serialized = serde_json::to_string(&config).expect("the config serializes");
        assert!(!serialized.contains("canary"));
        assert!(
            !serialized.contains("headers"),
            "the secret member is skipped when serializing: {serialized}"
        );
        Ok(())
    });
}

/// C-18. The pre-flight command is trustworthy: an invalid configuration exits `78` (`EX_CONFIG`)
/// and a valid one exits `0`.
///
/// This asserts the exit code every `ConfigError` carries and that a default configuration is
/// accepted; the process-level half of the command lives in `crates/http`.
#[test]
fn check_config_exits_78_on_invalid_and_0_on_valid() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__PUBLIC__BIND", "0.0.0.0:8080");
        let error = config::load(RuntimeRole::Scheduler).expect_err("V1 fires");
        assert_eq!(error.exit_code(), 78);
        Ok(())
    });

    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__ADMN__BIND", "127.0.0.1:1");
        let error = config::load(RuntimeRole::Scheduler).expect_err("an unknown key is fatal");
        assert_eq!(error.exit_code(), 78, "both arms exit EX_CONFIG");
        Ok(())
    });

    Jail::expect_with(|_| {
        config::load(RuntimeRole::Scheduler).expect("the defaults are valid, so this exits 0");
        Ok(())
    });
}
