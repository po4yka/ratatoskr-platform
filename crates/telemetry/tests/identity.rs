//! X-1 … X-5: the identity that observability and the wire share, and the types Platform must
//! never redeclare.
//!
//! Files under `tests/` are separate crates, so the workspace `unwrap_used` / `expect_used` denials
//! are relaxed here rather than through `clippy.toml`'s in-`cfg(test)` allowance.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use opentelemetry::Key;
use platform_core::RuntimeRole;
use platform_telemetry::identity;
use ratatoskr_event_envelope::ProducerName;

/// X-1 — ADR-0003: one producer identity for the whole bounded context, and it is the token
/// `contracts.toml [services].known` registers for this repository.
#[test]
fn the_service_name_is_the_single_registered_platform_producer_identity() {
    let producer = ProducerName::parse(identity::SERVICE_NAME)
        .expect("SERVICE_NAME must parse as a contracts ProducerName");

    assert_eq!(producer.as_str(), "ratatoskr-platform");
    assert_eq!(identity::SERVICE_NAME, "ratatoskr-platform");
}

/// X-2 — ADR-0003: the `role` label set can never become unbounded.
#[test]
fn the_runtime_role_label_set_has_exactly_three_values() {
    let labels: Vec<&str> = RuntimeRole::ALL.iter().map(|role| role.as_str()).collect();

    assert_eq!(labels, ["edge", "ingest", "scheduler"]);

    let mut unique = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), labels.len(), "role labels must be distinct");
}

/// X-3 — ADR-0003: observability and wire identity read the same constant, so they cannot drift.
#[test]
fn the_otel_resource_service_name_is_the_same_constant_as_the_producer_name() {
    for role in RuntimeRole::ALL {
        let resource = identity::resource(role);

        assert_eq!(
            resource
                .get(&Key::from_static_str("service.name"))
                .map(|v| v.to_string()),
            Some(identity::SERVICE_NAME.to_owned()),
        );
        assert_eq!(
            resource
                .get(&Key::from_static_str("service.namespace"))
                .map(|v| v.to_string()),
            Some("ratatoskr".to_owned()),
        );
        assert_eq!(
            resource
                .get(&Key::from_static_str("service.version"))
                .map(|v| v.to_string()),
            Some(identity::VERSION.to_owned()),
        );
        assert_eq!(
            resource
                .get(&Key::from_static_str("ratatoskr.runtime_role"))
                .map(|v| v.to_string()),
            Some(role.as_str().to_owned()),
            "dashboards facet on ratatoskr.runtime_role",
        );
    }
}

/// X-4 — `rust-toolchain.toml` and `Cargo.toml` cannot drift.
#[test]
fn the_declared_rust_version_is_the_prefix_of_the_pinned_toolchain_channel() {
    let pinned = std::fs::read_to_string(workspace_root().join("rust-toolchain.toml"))
        .expect("rust-toolchain.toml must exist at the workspace root");

    let channel = pinned
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim() == "channel")
        .map(|(_, value)| value.trim().trim_matches('"').to_owned())
        .expect("rust-toolchain.toml must pin a channel");

    assert!(
        channel.starts_with(identity::RUST_VERSION),
        "toolchain channel {channel} does not start with the declared rust-version {}",
        identity::RUST_VERSION,
    );
}

/// Every contracts type Platform consumes. A local declaration of any of these is the regression
/// this test exists to catch, permanently, without anyone remembering to look.
const CONTRACT_TYPES: [&str; 24] = [
    "EntityRef",
    "EntityKind",
    "EntityLocalId",
    "TenantRef",
    "BlobRef",
    "SafeMessage",
    "WireTimestamp",
    "Extensions",
    "CorrelationId",
    "EventId",
    "OperationId",
    "UserId",
    "ErrorCode",
    "ErrorEnvelope",
    "WarningEnvelope",
    "FieldPath",
    "FieldViolation",
    "TraceId",
    "EventEnvelope",
    "EventType",
    "EnvelopeSchemaVersion",
    "ProducerName",
    "OperationStatus",
    "OperationSnapshot",
];

/// X-5 — AGENTS.md "Use contracts from `ratatoskr-contracts`".
#[test]
fn no_shipped_contracts_type_is_redeclared() {
    let root = workspace_root();
    let mut sources = Vec::new();
    collect_rust_sources(&root.join("crates"), &mut sources);
    collect_rust_sources(&root.join("services"), &mut sources);

    assert!(
        !sources.is_empty(),
        "the source scan found no files, so it proves nothing"
    );

    for path in sources {
        let text = std::fs::read_to_string(&path).expect("a listed source file must be readable");
        for name in CONTRACT_TYPES {
            for keyword in ["struct", "enum", "union", "trait", "type"] {
                let declaration = format!("{keyword} {name}");
                assert!(
                    !text.contains(&declaration),
                    "{} redeclares the contracts type `{name}` as `{declaration}`",
                    path.display(),
                );
            }
        }
    }
}

/// The repository root, from this package's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Every `*.rs` file under a `src/` directory below `directory`. Test sources are excluded: they
/// legitimately name the types they check.
fn collect_rust_sources(directory: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, into);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path
                .components()
                .any(|component| component.as_os_str() == "src")
        {
            into.push(path);
        }
    }
}
