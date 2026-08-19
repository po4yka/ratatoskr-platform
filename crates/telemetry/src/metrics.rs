//! Every instrument this workspace emits, and nothing else.
//!
//! Prometheus pull on the admin listener. Metrics are **not** exported over OTLP: an OTLP metrics
//! pipeline discards every recording when no collector is running, so a developer would reasonably
//! think metrics work when they do not, whereas `curl localhost:9464/metrics` shows the truth.
//!
//! Cardinality is bounded by construction. `route` is always `axum::extract::MatchedPath`, never
//! the request URI, and an unmatched request is labelled [`UNMATCHED_ROUTE`]. `method` is always
//! one of the nine RFC 9110 tokens or [`OTHER_METHOD`], never the token the client sent. Those two
//! rules are the entire defence against a cardinality bomb an unauthenticated client can fire.
//!
//! Naming convention: HTTP server metrics follow the Prometheus/OTel `http_server_*` convention;
//! every other Platform metric is `platform_<subsystem>_<measure>[_<unit>]`, and every numeric name
//! carries a unit suffix. Future metric names are deliberately not pre-registered.

/// `http_server_request_duration_seconds{role,method,route,status}` — histogram.
///
/// Satisfies `ARCHITECTURE.md` S16 item 1 ENTIRELY: the derived `_count` IS the request count,
/// so there is no separate counter and therefore no second source of truth for one number.
pub const HTTP_SERVER_REQUEST_DURATION_SECONDS: &str = "http_server_request_duration_seconds";

/// `platform_readiness{role}` — gauge, `0` or `1`. `ARCHITECTURE.md` S16 item 9, the half that
/// has a subject at milestone 1.
pub const PLATFORM_READINESS: &str = "platform_readiness";

/// `platform_build_info{role,version,git_sha,rust_version}` — gauge, always `1`.
/// The first thing anyone looks at when a deployment misbehaves: what is actually running.
pub const PLATFORM_BUILD_INFO: &str = "platform_build_info";

/// `platform_scheduler_drift_seconds{schedule}` — gauge, seconds.
///
/// `ARCHITECTURE.md` S16 item 7, the drift half: how late the most recent occurrence of that
/// schedule was. A gauge rather than a histogram because a schedule publishes once per interval,
/// so "how late was the last one" is the whole question — and because the shared latency buckets
/// stop at ten seconds, which would put every interesting value in one overflow bucket.
///
/// The `schedule` label is bounded by the rows of `operations.schedules`, which no request path
/// writes: an operator inserts them, and `schedules_name_is_a_label` bounds each name to 64
/// characters of `[a-z0-9_-]`. It is not an attacker-reachable label.
pub const PLATFORM_SCHEDULER_DRIFT_SECONDS: &str = "platform_scheduler_drift_seconds";

/// `platform_scheduler_occurrences_total{schedule,outcome}` — counter.
///
/// `ARCHITECTURE.md` S16 item 7, the duplicate-suppression half. `outcome` is one of three
/// constants: `published`, `suppressed` — an occurrence whose identifier already existed, which is
/// the suppression being counted — and `skipped`, a grid point discarded by the schedule's
/// catch-up policy. A suppression that is never zero means something is republishing; a skip that
/// is never zero means the process is not keeping up with its own schedules.
pub const PLATFORM_SCHEDULER_OCCURRENCES_TOTAL: &str = "platform_scheduler_occurrences_total";

/// The `route` label of a request that matched no route template.
///
/// A constant, never the request URI: this is what closes the 404-scanning cardinality bomb.
pub const UNMATCHED_ROUTE: &str = "<unmatched>";

/// The `method` label of a request whose method is outside the RFC 9110 set.
///
/// The same device as [`UNMATCHED_ROUTE`], for the same reason: a method token is an
/// attacker-chosen string of unbounded length taken off the unauthenticated public listener, so
/// using it raw is a cardinality bomb an unauthenticated client can fire at will.
pub const OTHER_METHOD: &str = "<other>";

/// Latency buckets, in seconds.
pub const DURATION_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Every metric name this workspace emits. A rename silently breaks every dashboard and every
/// alert, so it must break test T-4 first.
pub const ALL: [&str; 5] = [
    HTTP_SERVER_REQUEST_DURATION_SECONDS,
    PLATFORM_READINESS,
    PLATFORM_BUILD_INFO,
    PLATFORM_SCHEDULER_DRIFT_SECONDS,
    PLATFORM_SCHEDULER_OCCURRENCES_TOTAL,
];
