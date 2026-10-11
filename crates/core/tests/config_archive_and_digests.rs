//! The archive size ceiling (rule V21) and the optional channel-digests surface (rule V22).
//!
//! Kept beside `config_validation.rs` rather than in it: that file is at the repository's line
//! limit, and these rules were added together with the routes that read them.

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

use figment::Jail;
use platform_core::RuntimeRole;
use platform_core::config::{self, ConfigError, Violation};

/// The violations `load` reports for `role`, or a panic if the configuration was accepted.
fn violations(role: RuntimeRole) -> Vec<Violation> {
    match config::load(role) {
        Ok(config) => panic!("{role} must reject this configuration, got {config:?}"),
        Err(ConfigError::Invalid(found)) => found,
        Err(other) => panic!("expected a semantic failure, got {other}"),
    }
}

fn names(found: &[Violation], key: &str) -> bool {
    found.iter().any(|violation| violation.key == key)
}

/// V21. The ceiling is its own setting: two gibibytes unless configured, and a value outside
/// one mebibyte to ten gibibytes is a startup error that names the key.
#[test]
fn archive_ceiling_defaults_to_two_gibibytes_and_rejects_out_of_range() {
    Jail::expect_with(|_jail| {
        let loaded = config::load(RuntimeRole::Edge).expect("defaults alone must be valid");
        assert_eq!(loaded.archive_staging.max_archive_bytes, 2_147_483_648);
        Ok(())
    });

    for refused in [0_u64, 1_048_575, 10_737_418_241] {
        Jail::expect_with(|jail| {
            jail.set_env("RATATOSKR__ARCHIVE_STAGING__MAX_ARCHIVE_BYTES", refused);
            assert!(
                names(
                    &violations(RuntimeRole::Edge),
                    "archive_staging.max_archive_bytes"
                ),
                "{refused} must be refused by rule V21"
            );
            Ok(())
        });
    }

    for accepted in [1_048_576_u64, 10_737_418_240] {
        Jail::expect_with(|jail| {
            jail.set_env("RATATOSKR__ARCHIVE_STAGING__MAX_ARCHIVE_BYTES", accepted);
            let loaded = config::load(RuntimeRole::Edge).expect("the bounds of V21 are legal");
            assert_eq!(loaded.archive_staging.max_archive_bytes, accepted);
            Ok(())
        });
    }
}

const DIGESTS_SECRET: &str = "channel-digests-secret-value-0123456789";

/// V22. The digest API is a loopback listener on 8098 plus a bearer secret, and the two come together
/// or not at all.
#[test]
fn channel_digests_requires_loopback_8098_and_a_secret_together() {
    // Absent is valid: the command routes work without it.
    Jail::expect_with(|_jail| {
        let loaded = config::load(RuntimeRole::Edge).expect("defaults alone must be valid");
        assert!(loaded.channel_digests.is_none());
        Ok(())
    });

    // Both, on the allocated loopback port.
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__LISTENER", "127.0.0.1:8098");
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__SERVICE_SECRET", DIGESTS_SECRET);
        let loaded = config::load(RuntimeRole::Edge).expect("the allocated listener is valid");
        let digests = loaded.channel_digests.expect("the section is present");
        assert_eq!(digests.listener.to_string(), "127.0.0.1:8098");
        Ok(())
    });

    // A listener without a secret, and a secret without a listener.
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__LISTENER", "127.0.0.1:8098");
        assert!(
            names(
                &violations(RuntimeRole::Edge),
                "channel_digests.service_secret"
            ),
            "a listener alone must be refused"
        );
        Ok(())
    });
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__SERVICE_SECRET", DIGESTS_SECRET);
        assert!(
            config::load(RuntimeRole::Edge).is_err(),
            "a secret alone must be refused"
        );
        Ok(())
    });

    // Not loopback, wrong port, empty and oversized secrets.
    for (listener, secret, key) in [
        ("10.0.0.5:8098", DIGESTS_SECRET, "channel_digests.listener"),
        ("0.0.0.0:8098", DIGESTS_SECRET, "channel_digests.listener"),
        ("127.0.0.1:8099", DIGESTS_SECRET, "channel_digests.listener"),
        ("127.0.0.1:8098", "", "channel_digests.service_secret"),
    ] {
        Jail::expect_with(|jail| {
            jail.set_env("RATATOSKR__CHANNEL_DIGESTS__LISTENER", listener);
            jail.set_env("RATATOSKR__CHANNEL_DIGESTS__SERVICE_SECRET", secret);
            assert!(
                names(&violations(RuntimeRole::Edge), key),
                "{listener} with a secret of {} bytes must violate {key}",
                secret.len()
            );
            Ok(())
        });
    }
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__LISTENER", "127.0.0.1:8098");
        jail.set_env(
            "RATATOSKR__CHANNEL_DIGESTS__SERVICE_SECRET",
            "s".repeat(4097),
        );
        assert!(names(
            &violations(RuntimeRole::Edge),
            "channel_digests.service_secret"
        ));
        Ok(())
    });

    // Only Edge talks to the digest service.
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__LISTENER", "127.0.0.1:8098");
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__SERVICE_SECRET", DIGESTS_SECRET);
        jail.set_env("RATATOSKR__PUBLIC__BIND", "127.0.0.1:8181");
        assert!(names(&violations(RuntimeRole::Ingest), "channel_digests"));
        Ok(())
    });
}

/// V22. The secret is a `SecretString`: it does not appear in the Debug output of the loaded
/// configuration or in its serialized form, which is what `check-config` writes.
#[test]
fn secret_is_redacted_in_debug_and_check_config() {
    Jail::expect_with(|jail| {
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__LISTENER", "127.0.0.1:8098");
        jail.set_env("RATATOSKR__CHANNEL_DIGESTS__SERVICE_SECRET", DIGESTS_SECRET);
        let loaded = config::load(RuntimeRole::Edge).expect("a valid configuration");

        let debug = format!("{loaded:?}");
        assert!(!debug.contains(DIGESTS_SECRET), "Debug leaked the secret");
        assert!(
            debug.contains("REDACTED"),
            "Debug should say it redacted: {debug}"
        );
        let serialized = serde_json::to_string(&loaded).expect("a serialized configuration");
        assert!(
            !serialized.contains(DIGESTS_SECRET),
            "the serialized configuration leaked the secret"
        );
        Ok(())
    });
}

/// A chatgpt and a claude route on the wrong ports, built as the figment defaults serialise them.
fn misrouted_archive_receivers() -> platform_core::config::PlatformConfig {
    use platform_core::config::{GatewayRouteClass, GatewayRouteConfig, PlatformConfig};

    let mut configured = PlatformConfig::defaults(RuntimeRole::Edge);
    for (service, prefix, listener) in [
        ("chatgpt", "/v1/chatgpt", "127.0.0.1:9096"),
        ("claude", "/v1/claude", "127.0.0.1:9097"),
    ] {
        configured.gateway.routes.insert(
            service.to_owned(),
            GatewayRouteConfig {
                prefix: prefix.to_owned(),
                listener: listener.parse().expect("a socket address"),
                class: Some(GatewayRouteClass::Transfer),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        );
    }
    configured
}

/// XR-021 R3-13. A route violation says which route failed. Seven identical `RATATOSKR__GATEWAY__ROUTES`
/// entries were all the Round 2 smoke could show an operator.
#[test]
fn a_misrouted_gateway_route_is_named_in_its_violation() {
    use figment::Figment;
    use figment::providers::Serialized;

    let Err(ConfigError::Invalid(found)) = config::load_from(
        RuntimeRole::Edge,
        Figment::from(Serialized::defaults(misrouted_archive_receivers())),
    ) else {
        panic!("misrouted receivers must be refused");
    };

    let chatgpt: Vec<&Violation> = found
        .iter()
        .filter(|violation| violation.key == "gateway.routes.chatgpt.listener")
        .collect();
    assert_eq!(chatgpt.len(), 1, "{found:?}");
    assert_eq!(
        chatgpt[0].env_var,
        "RATATOSKR__GATEWAY__ROUTES__CHATGPT__LISTENER"
    );
}

/// XR-021 R3-13. Two failed routes are two distinct entries, one per failed field.
#[test]
fn two_failed_routes_yield_two_distinct_entries() {
    use figment::Figment;
    use figment::providers::Serialized;

    let Err(ConfigError::Invalid(found)) = config::load_from(
        RuntimeRole::Edge,
        Figment::from(Serialized::defaults(misrouted_archive_receivers())),
    ) else {
        panic!("misrouted receivers must be refused");
    };

    let listeners: Vec<(&str, &str)> = found
        .iter()
        .filter(|violation| violation.key.ends_with(".listener"))
        .map(|violation| (&*violation.key, &*violation.env_var))
        .collect();
    assert_eq!(
        listeners,
        [
            (
                "gateway.routes.chatgpt.listener",
                "RATATOSKR__GATEWAY__ROUTES__CHATGPT__LISTENER"
            ),
            (
                "gateway.routes.claude.listener",
                "RATATOSKR__GATEWAY__ROUTES__CLAUDE__LISTENER"
            ),
        ]
    );
}
