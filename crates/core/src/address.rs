//! What Platform will hand on as an address.
//!
//! One implementation, in the crate both serving crates already depend on, and deliberately not
//! duplicated beside each route. `POST /v2/captures` and `POST /v2/ingest/webhooks/{id}` produce
//! the same command for the same consumer, so they must accept the same addresses: if one of them
//! tightened its policy and the other did not, the looser door would be a way to submit something
//! the other refuses, which is a privilege difference nobody chose.

/// The longest address this API accepts, in characters.
///
/// `ARCHITECTURE.md` S14 bounds every inbound value. 2048 is the ceiling every mainstream browser
/// and proxy already enforces, so a longer address is one that could not have been produced by a
/// client and would fail somewhere further along regardless.
pub const MAX_URL: usize = 2048;

/// Whether Platform will hand this address to the service that fetches it.
///
/// Deliberately shallow. `ARCHITECTURE.md` S15 says Edge "does not render or inspect active
/// content", and Platform never opens the connection: the real defence — SSRF policy, redirect
/// handling, response limits — belongs to `ratatoskr-extractor`, which is the process that does.
/// Rejecting an obviously unusable scheme here only avoids creating an operation that can only
/// fail.
#[must_use]
pub fn is_capturable(raw: &str) -> bool {
    if raw.len() > MAX_URL {
        return false;
    }
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.host().is_some()
}
