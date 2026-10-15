//! V24 — the sites gateway's source address (workspace change `add-site-gateway-hosting`).

use std::net::{IpAddr, Ipv4Addr};

use super::Violation;
use crate::config::model::PlatformConfig;

/// V24 — Edge reads `X-Ratatoskr-Client-IP` only on a connection from `sites.gateway_peer`, so the
/// address must be a loopback address of this host, and not `127.0.0.1`, which is where
/// `cloudflared` and every other local client connect from.
pub(super) fn gateway_peer_violations(config: &PlatformConfig) -> Vec<Violation> {
    let peer = config.sites.gateway_peer;
    if peer.is_loopback() && peer != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Vec::new();
    }
    vec![Violation {
        key: "sites.gateway_peer".into(),
        env_var: "RATATOSKR__SITES__GATEWAY_PEER".into(),
        rule: "must be a loopback address other than 127.0.0.1, the source of cloudflared (V24)",
    }]
}
