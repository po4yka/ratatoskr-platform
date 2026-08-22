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

/// `platform_capability_available{capability}` — gauge, `0` or `1`. `ARCHITECTURE.md` S16 item 9,
/// the capability half.
///
/// Published on a timer rather than from the `GET /v1/capabilities` handler. A capability is a pure
/// function of the deployment's own state (ADR-0008), so its value is knowable whether or not a
/// client asks — and a gauge that only moves when somebody asks reports the state of the last
/// question, not the state of the deployment. The one input that changes at run time is the
/// database probe, which is the same fact `/health/ready` reports.
///
/// The `capability` label is `Capability::ALL`, a closed set the compiler counts.
pub const PLATFORM_CAPABILITY_AVAILABLE: &str = "platform_capability_available";

/// `platform_auth_decisions_total{action,outcome}` — counter. `ARCHITECTURE.md` S16 item 2.
///
/// Emitted from `platform_identity::audit::record`, so the counter and `identity.audit_events`
/// cannot disagree about what was decided: there is one call, and it does both. `outcome` is
/// `allowed` or `denied`; `action` is the dotted name the call site passes, and every call site
/// passes a constant.
///
/// It carries no identifier of any kind — S16 requires these outcomes "without sensitive
/// identifiers", and a user id in a label is both a disclosure and an unbounded cardinality.
pub const PLATFORM_AUTH_DECISIONS_TOTAL: &str = "platform_auth_decisions_total";

/// `platform_rate_limit_decisions_total{outcome}` — counter.
///
/// Emitted from the per-actor limiter's `admit`, where the decision is made, so every call site is
/// counted by construction rather than by a line somebody remembers to add. `outcome` is
/// `admitted` or `refused` — a closed set, like every label here.
///
/// It carries no identifier of any kind: an actor id in a label is both a disclosure and an
/// unbounded cardinality, and S16 requires these decisions "without sensitive identifiers".
pub const PLATFORM_RATE_LIMIT_DECISIONS_TOTAL: &str = "platform_rate_limit_decisions_total";

/// `platform_operation_transitions_total{outcome}` — counter. `ARCHITECTURE.md` S16 item 3, the
/// transition half.
///
/// `outcome` is one of the four `Transition` variants: `advance`, `duplicate`, `stale`, `conflict`.
/// `crates/operations/src/transition.rs` has described two of them as "a no-op plus a counter" and
/// "ignored plus a counter" since milestone 3; this is that counter.
///
/// The one to alarm on is `conflict`: a duplicate and a late older status are ordinary traffic
/// under at-least-once delivery, and two producers disagreeing about a terminal outcome is a defect.
pub const PLATFORM_OPERATION_TRANSITIONS_TOTAL: &str = "platform_operation_transitions_total";

/// `platform_operations{status}` — gauge, how many operations are in each status.
///
/// `status` is the closed set of seven. Sampled on the observer's timer, so it is a snapshot and
/// not a running total; the counter above is the running total.
pub const PLATFORM_OPERATIONS: &str = "platform_operations";

/// `platform_operations_oldest_unterminated_age_seconds` — gauge. `ARCHITECTURE.md` S16 item 3, the
/// age half.
///
/// The age of the oldest operation that has not reached a terminal status. A count alone cannot
/// distinguish a busy system from a stuck one, which is the same reason the outbox exports an age
/// beside its depth. This is the number the stale-operation reconciler S14 asks for would act on,
/// and it is worth having before that exists: today it is the only way to see one.
pub const PLATFORM_OPERATIONS_OLDEST_UNTERMINATED_AGE_SECONDS: &str =
    "platform_operations_oldest_unterminated_age_seconds";

/// `platform_outbox_pending` — gauge. `ARCHITECTURE.md` S16 item 4.
pub const PLATFORM_OUTBOX_PENDING: &str = "platform_outbox_pending";

/// `platform_outbox_dead_lettered` — gauge.
///
/// Rows that exhausted their attempts. Any value above zero is work a client was told had been
/// accepted and that nobody will retry, so this is an alert rather than a dashboard number.
pub const PLATFORM_OUTBOX_DEAD_LETTERED: &str = "platform_outbox_dead_lettered";

/// `platform_outbox_oldest_pending_age_seconds` — gauge. The lag an operator alarms on: a depth
/// alone cannot tell a busy queue from a stopped publisher.
pub const PLATFORM_OUTBOX_OLDEST_PENDING_AGE_SECONDS: &str =
    "platform_outbox_oldest_pending_age_seconds";

/// `platform_inbox_unprocessed` — gauge. `ARCHITECTURE.md` S16 item 4, the inbox half.
///
/// Messages claimed by the consumer and not finished. A non-zero value that does not fall is a
/// handler that is failing after the inbox row was written, which the consumer's own error log
/// shows once and this shows continuously.
pub const PLATFORM_INBOX_UNPROCESSED: &str = "platform_inbox_unprocessed";

/// `platform_outbox_publications_total{outcome}` — counter. `ARCHITECTURE.md` S16 item 5.
///
/// `outcome` is `published`, `failed` or `dead_lettered`. Emitted from `pump::run_once`, beside the
/// place each number is decided, so a pass cannot report one thing and count another.
///
/// `failed` rising while `published` also rises is a flapping broker; `failed` rising alone is a
/// broker that is gone — or a NATS publish permission that denies the subject, which arrives at the
/// client as the same unacknowledged publish (`deploy/nats/ratatoskr.conf`).
pub const PLATFORM_OUTBOX_PUBLICATIONS_TOTAL: &str = "platform_outbox_publications_total";

/// `platform_idempotency_outcomes_total{outcome}` — counter. `ARCHITECTURE.md` S16 item 8.
///
/// `outcome` is `proceed` — a first attempt — `replay`, a retry of the same request returning the
/// original operation, or `refuse`, the same key with a different body or an attempt still in
/// flight. Emitted from `reserve`, which is the one statement that decides which of the three it is.
pub const PLATFORM_IDEMPOTENCY_OUTCOMES_TOTAL: &str = "platform_idempotency_outcomes_total";

/// `platform_sse_connections` — gauge. `ARCHITECTURE.md` S16 item 6, the connection half.
///
/// Incremented when a stream starts and decremented when it ends, by a guard whose `Drop` runs on
/// every exit including a client that vanishes — which is the exit that matters, because it is the
/// one no code path returns through.
pub const PLATFORM_SSE_CONNECTIONS: &str = "platform_sse_connections";

/// `platform_sse_delivery_lag_seconds` — gauge. `ARCHITECTURE.md` S16 item 6, the delivery half.
///
/// The age of the most recent progress entry at the moment it was written to a stream: how long a
/// client waited for a fact that was already recorded. It is bounded below by the stream's own poll
/// interval and is the number that says whether that interval is too long.
pub const PLATFORM_SSE_DELIVERY_LAG_SECONDS: &str = "platform_sse_delivery_lag_seconds";

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
pub const ALL: [&str; 19] = [
    HTTP_SERVER_REQUEST_DURATION_SECONDS,
    PLATFORM_READINESS,
    PLATFORM_BUILD_INFO,
    PLATFORM_SCHEDULER_DRIFT_SECONDS,
    PLATFORM_SCHEDULER_OCCURRENCES_TOTAL,
    PLATFORM_CAPABILITY_AVAILABLE,
    PLATFORM_AUTH_DECISIONS_TOTAL,
    PLATFORM_RATE_LIMIT_DECISIONS_TOTAL,
    PLATFORM_OPERATION_TRANSITIONS_TOTAL,
    PLATFORM_OPERATIONS,
    PLATFORM_OPERATIONS_OLDEST_UNTERMINATED_AGE_SECONDS,
    PLATFORM_OUTBOX_PENDING,
    PLATFORM_OUTBOX_DEAD_LETTERED,
    PLATFORM_OUTBOX_OLDEST_PENDING_AGE_SECONDS,
    PLATFORM_INBOX_UNPROCESSED,
    PLATFORM_OUTBOX_PUBLICATIONS_TOTAL,
    PLATFORM_IDEMPOTENCY_OUTCOMES_TOTAL,
    PLATFORM_SSE_CONNECTIONS,
    PLATFORM_SSE_DELIVERY_LAG_SECONDS,
];
