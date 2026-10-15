//! Which request paths Edge forwards to a domain service (workspace change
//! `add-site-publications`, task 4.4).
//!
//! Edge forwards a path unchanged, and a service may receive encoded dots and separators in its own
//! segments (a notes file path does). A domain service serves its internal routes under `/internal/`
//! on the same listener, so Edge refuses a path that names `internal` directly below its route
//! prefix, or that leaves the prefix, once its dot segments are resolved with `%2e`, `%2f`, `%5c`
//! and a literal `\` read as `.`, `/`, `/` and `/`. A server or a library that resolves such a path
//! can then never reach an internal route from the public listener.

/// The segments of `path` below `prefix` with its dot segments resolved, or `None` when they leave
/// the prefix.
fn resolved(prefix: &str, path: &str) -> Option<Vec<String>> {
    let below = path.strip_prefix(prefix).unwrap_or(path);
    let decoded = below
        .to_ascii_lowercase()
        .replace("%2e", ".")
        .replace("%2f", "/")
        .replace("%5c", "/")
        .replace('\\', "/");
    let mut kept: Vec<String> = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                kept.pop()?;
            }
            other => kept.push(other.to_owned()),
        }
    }
    Some(kept)
}

/// Whether Edge forwards `path` to the route whose prefix is `prefix`. A refused path is answered as
/// an unknown route.
pub(crate) fn forwardable(prefix: &str, path: &str) -> bool {
    resolved(prefix, path).is_some_and(|kept| kept.first().map(String::as_str) != Some("internal"))
}

/// Whether `path`, resolved below `/v1/sites`, still names `site` as its first segment (review L2
/// of SITE-027): Edge checked the site of the path it routed, so a path that resolves to another
/// site is answered as an unknown route.
pub(crate) fn names_site(path: &str, site: &str) -> bool {
    resolved("/v1/sites", path).is_some_and(|kept| kept.first().map(String::as_str) == Some(site))
}
