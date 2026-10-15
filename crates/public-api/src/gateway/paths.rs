//! Which request paths Edge forwards to a domain service (workspace change
//! `add-site-publications`, task 4.4).
//!
//! Edge forwards a path unchanged, and a service may receive encoded dots and separators in its own
//! segments (a notes file path does). A domain service serves its internal routes under `/internal/`
//! on the same listener, so Edge refuses a path that names `internal` directly below its route
//! prefix, or that leaves the prefix, once its dot segments are resolved with `%2e`, `%2f` and `%5c`
//! read as `.`, `/` and `/`. A server or a library that resolves such a path can then never reach an
//! internal route from the public listener.

/// Whether Edge forwards `path` to the route whose prefix is `prefix`. A refused path is answered as
/// an unknown route.
pub(crate) fn forwardable(prefix: &str, path: &str) -> bool {
    let below = path.strip_prefix(prefix).unwrap_or(path);
    let decoded = below
        .to_ascii_lowercase()
        .replace("%2e", ".")
        .replace("%2f", "/")
        .replace("%5c", "/");
    let mut kept: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if kept.pop().is_none() {
                    return false;
                }
            }
            other => kept.push(other),
        }
    }
    kept.first() != Some(&"internal")
}
