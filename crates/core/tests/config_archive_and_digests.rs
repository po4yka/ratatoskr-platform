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
