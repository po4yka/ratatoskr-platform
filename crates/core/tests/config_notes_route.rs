//! Startup validation of the notes route: the public limits and the transfer budget must hold every
//! budget and every body limit that a notes path selects (workspace change
//! `add-notes-vault-connections`, tasks 9.10 and 9.30).

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]
#![allow(
    clippy::result_large_err,
    reason = "the loader closures return ConfigError; the size is the loader's, not ours"
)]

use figment::Figment;
use figment::providers::Serialized;
use platform_core::RuntimeRole;
use platform_core::config::{
    self, ConfigError, GatewayRouteClass, GatewayRouteConfig, PlatformConfig, Violation,
};

/// Whether a violation set names `key`.
fn names(found: &[Violation], key: &str) -> bool {
    found.iter().any(|violation| violation.key == key)
}

/// The notes route is `control`, and its blob path takes the `transfer` budget, so the public
/// limits must hold the transfer budget too.
#[test]
fn notes_blob_path_needs_the_public_transfer_limits() {
    let load = |max_body_bytes, request_timeout_seconds| {
        let mut configured = PlatformConfig::defaults(RuntimeRole::Edge);
        let public = configured.public.as_mut().expect("a public listener");
        public.max_body_bytes = max_body_bytes;
        public.request_timeout_seconds = request_timeout_seconds;
        configured.gateway.routes.insert(
            "notes".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/notes".to_owned(),
                listener: "127.0.0.1:8099".parse().unwrap(),
                class: Some(GatewayRouteClass::Control),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        );
        config::load_from(
            RuntimeRole::Edge,
            Figment::from(Serialized::defaults(configured)),
        )
    };

    let Err(ConfigError::Invalid(found)) = load(1_048_576, 15) else {
        panic!("public limits that hold only the control budget must be refused");
    };
    assert!(names(&found, "public.max_body_bytes"), "{found:?}");
    assert!(names(&found, "public.request_timeout_seconds"), "{found:?}");
    load(104_857_600, 300).expect("public limits that hold the transfer budget are valid");
}

/// Workspace task 9.30: a transfer budget below the largest body limit of a notes sub-path (the
/// manifest, 32 MiB) would lower that limit in silence, and notes would never see a valid manifest.
#[test]
fn notes_route_needs_a_transfer_budget_that_holds_its_body_limits() {
    let load = |transfer_max_body_bytes| {
        let mut configured = PlatformConfig::defaults(RuntimeRole::Edge);
        let public = configured.public.as_mut().expect("a public listener");
        public.max_body_bytes = 104_857_600;
        public.request_timeout_seconds = 300;
        configured.gateway.budgets.transfer.max_body_bytes = transfer_max_body_bytes;
        configured.gateway.routes.insert(
            "notes".to_owned(),
            GatewayRouteConfig {
                prefix: "/v1/notes".to_owned(),
                listener: "127.0.0.1:8099".parse().unwrap(),
                class: Some(GatewayRouteClass::Control),
                capabilities_path: "/v1/capabilities".to_owned(),
                archive_receipt_path: "/v1/ai-archives/receipt".to_owned(),
            },
        );
        config::load_from(
            RuntimeRole::Edge,
            Figment::from(Serialized::defaults(configured)),
        )
    };

    let Err(ConfigError::Invalid(found)) = load(33_554_431) else {
        panic!("a transfer budget below the 32 MiB manifest limit of notes must be refused");
    };
    assert!(
        names(&found, "gateway.budgets.transfer.max_body_bytes"),
        "{found:?}"
    );
    load(33_554_432).expect("a transfer budget that holds the manifest limit is valid");
}
