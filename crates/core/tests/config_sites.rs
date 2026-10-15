//! Startup validation of `sites.gateway_peer` (workspace change `add-site-gateway-hosting`, spec
//! `site-gateway`): Edge reads `X-Ratatoskr-Client-IP` only on a connection from this address, so
//! the address must be a loopback address that no other local client uses. `cloudflared` reaches
//! Edge from `127.0.0.1`.

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
use platform_core::config::{self, ConfigError, PlatformConfig};

fn load(peer: &str) -> Result<PlatformConfig, ConfigError> {
    let mut configured = PlatformConfig::defaults(RuntimeRole::Edge);
    configured.sites.gateway_peer = peer.parse().expect("an IP address");
    config::load_from(
        RuntimeRole::Edge,
        Figment::from(Serialized::defaults(configured)),
    )
}

/// The tunnel's source address and an address off the host are refused at start-up, and the
/// refusal names the key; the default and another loopback address start.
#[test]
fn sites_gateway_peer_is_loopback_and_not_the_tunnel() {
    for peer in ["127.0.0.1", "10.0.0.5", "0.0.0.0"] {
        let Err(ConfigError::Invalid(found)) = load(peer) else {
            panic!("the gateway peer {peer} must be refused at start-up");
        };
        assert!(
            found
                .iter()
                .any(|violation| violation.key == "sites.gateway_peer"
                    && violation.env_var == "RATATOSKR__SITES__GATEWAY_PEER"),
            "the refusal of {peer} names the key: {found:?}"
        );
    }
    for peer in ["127.0.0.83", "127.0.0.84"] {
        load(peer).unwrap_or_else(|error| panic!("the gateway peer {peer} is valid: {error:?}"));
    }
}
