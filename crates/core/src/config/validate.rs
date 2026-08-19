//! The startup validation rules V1–V10 and the operator-facing failure report.
//!
//! Order at startup is strictly: extract, validate, initialise telemetry, bind listeners. Telemetry
//! is initialised *after* validation so that an invalid `log_filter` fails as a configuration
//! problem on stderr rather than inside subscriber setup, where nothing could report it.
//!
//! figment's own extraction is fail-fast, so the "report every problem" guarantee comes from this
//! pass and not from serde.

use std::fmt::Write as _;

use figment::error::Kind;
use tracing_subscriber::EnvFilter;

use crate::config::model::PlatformConfig;
use secrecy::ExposeSecret;

use crate::role::RuntimeRole;

/// One startup-rule violation.
///
/// Every member is `&'static str`. It is therefore STRUCTURALLY IMPOSSIBLE for a supplied value to
/// appear in a configuration failure report, so the report can never echo a secret. This is a type
/// property, not a rule someone has to remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The dotted configuration path, e.g. `public.bind`.
    pub key: &'static str,
    /// The environment variable that sets it, e.g. `RATATOSKR__PUBLIC__BIND`.
    pub env_var: &'static str,
    /// What the rule requires, and the document that requires it.
    pub rule: &'static str,
}

/// The key and variable of the public listener address, named by V1 and V2.
const PUBLIC_BIND: (&str, &str) = ("public.bind", "RATATOSKR__PUBLIC__BIND");

/// Applies V1–V10 and returns every violation found, in rule order.
pub(crate) fn validate(role: RuntimeRole, config: &PlatformConfig) -> Vec<Violation> {
    let mut found = Vec::new();

    // V1 — the role decides whether a public listener may exist at all (ARCHITECTURE.md S18).
    match (role.may_have_public_listener(), config.public.as_ref()) {
        (true, None) => found.push(Violation {
            key: PUBLIC_BIND.0,
            env_var: PUBLIC_BIND.1,
            rule: required_public_listener(role),
        }),
        (false, Some(_)) => found.push(Violation {
            key: PUBLIC_BIND.0,
            env_var: PUBLIC_BIND.1,
            rule: forbidden_public_listener(role),
        }),
        _ => {}
    }

    if let Some(public) = config.public.as_ref() {
        // V2 — one listener would silently win and /metrics would be published on the public port.
        if public.bind == config.admin.bind {
            found.push(Violation {
                key: PUBLIC_BIND.0,
                env_var: PUBLIC_BIND.1,
                rule: "must not equal admin.bind; the operator plane and the public surface are \
                       separate listeners (AGENTS.md)",
            });
        }

        // V3
        if !(1..=300).contains(&public.request_timeout_seconds) {
            found.push(Violation {
                key: "public.request_timeout_seconds",
                env_var: "RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS",
                rule: "must be 1..=300 (ARCHITECTURE.md S5.2)",
            });
        }

        // V4
        if !(1024..=104_857_600).contains(&public.max_body_bytes) {
            found.push(Violation {
                key: "public.max_body_bytes",
                env_var: "RATATOSKR__PUBLIC__MAX_BODY_BYTES",
                rule: "must be 1024..=104857600 (ARCHITECTURE.md S14)",
            });
        }
    }

    // V5 — a bad filter otherwise silences every log line at the moment you need them.
    if EnvFilter::try_new(&config.telemetry.log_filter).is_err() {
        found.push(Violation {
            key: "telemetry.log_filter",
            env_var: "RATATOSKR__TELEMETRY__LOG_FILTER",
            rule: "must parse as a tracing-subscriber EnvFilter directive string, e.g. \
                   info,tower_http=info",
        });
    }

    // V6 — a total above the pod termination grace period guarantees SIGKILL mid-request.
    let drain = config.shutdown.drain_seconds;
    let grace = config.shutdown.grace_seconds;
    if drain > 60 {
        found.push(Violation {
            key: "shutdown.drain_seconds",
            env_var: "RATATOSKR__SHUTDOWN__DRAIN_SECONDS",
            rule: "must be 0..=60, and drain_seconds + grace_seconds must not exceed 120",
        });
    }
    if !(1..=120).contains(&grace) || drain.saturating_add(grace) > 120 {
        found.push(Violation {
            key: "shutdown.grace_seconds",
            env_var: "RATATOSKR__SHUTDOWN__GRACE_SECONDS",
            rule: "must be 1..=120, and drain_seconds + grace_seconds must not exceed 120",
        });
    }

    found.extend(otlp_violations(config));

    found.extend(database_violations(config));
    found.extend(bus_violations(config));

    found
}

/// V7 to V10 — the OTLP exporter rules.
///
/// Extracted for the same reason as the database and bus rules: one subsystem, one function, and
/// [`validate`] stays inside the workspace's function-length lint.
fn otlp_violations(config: &PlatformConfig) -> Vec<Violation> {
    let mut found = Vec::new();
    if let Some(otlp) = config.telemetry.otlp.as_ref() {
        // V7
        let scheme_ok = matches!(otlp.endpoint.scheme(), "http" | "https");
        if !scheme_ok || otlp.endpoint.host().is_none() {
            found.push(Violation {
                key: "telemetry.otlp.endpoint",
                env_var: "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
                rule: "must be an http or https URL with a host",
            });
        }

        // V8
        if !(1..=60).contains(&otlp.timeout_seconds) {
            found.push(Violation {
                key: "telemetry.otlp.timeout_seconds",
                env_var: "RATATOSKR__TELEMETRY__OTLP__TIMEOUT_SECONDS",
                rule: "must be 1..=60",
            });
        }

        // V9 — a header name containing a control character is a request-splitting primitive.
        if !otlp.headers.keys().all(|name| is_header_name(name)) {
            found.push(Violation {
                key: "telemetry.otlp.headers",
                env_var: "RATATOSKR__TELEMETRY__OTLP__HEADERS__<NAME>",
                rule: "every header name must match ^[a-z0-9-]{1,64}$",
            });
        }

        // V10 — `Url` is the second credential carrier in this struct, and the only one that is not
        // a `SecretString`: its `Debug` prints `username`, `password` and `query` as plain fields,
        // and the whole configuration is rendered with `Debug` into the effective-configuration
        // INFO line and into `check-config`'s output. `https://<token>@collector` and
        // `?access_token=…` are the standard forms for several OTLP vendors, so this is a hole an
        // operator falls into by following a vendor's own instructions. The header map is the one
        // place a collector credential may live (AGENTS.md; SECURITY.md "redact secrets").
        if !otlp.endpoint.username().is_empty()
            || otlp.endpoint.password().is_some()
            || otlp.endpoint.query().is_some()
        {
            found.push(Violation {
                key: "telemetry.otlp.endpoint",
                env_var: "RATATOSKR__TELEMETRY__OTLP__ENDPOINT",
                rule: "must not embed a user name, a password or a query string; a collector \
                       credential belongs in telemetry.otlp.headers, which cannot be printed \
                       (SECURITY.md)",
            });
        }
    }

    found
}

/// V13 — the bus rules.
///
/// Extracted for the same reason as the database rules: [`validate`] stays inside the workspace's
/// function-length lint, and the split follows a subsystem boundary rather than a line count.
fn bus_violations(config: &PlatformConfig) -> Vec<Violation> {
    let mut found = Vec::new();
    let Some(bus) = &config.bus else {
        return found;
    };

    if !matches!(bus.url.scheme(), "nats" | "tls" | "ws" | "wss") {
        found.push(Violation {
            key: "bus.url",
            env_var: "RATATOSKR__BUS__URL",
            rule: "must be a nats://, tls://, ws:// or wss:// URL",
        });
    }

    // The same credential-in-a-URL hole rule V10 closes for the collector. A NATS URL prints in the
    // effective-configuration line, so a credential in it is a credential in the log.
    if !bus.url.username().is_empty() || bus.url.password().is_some() {
        found.push(Violation {
            key: "bus.url",
            env_var: "RATATOSKR__BUS__URL",
            rule: "must not embed a user name or a password; a bus credential belongs in a \
                   credentials file read by path (SECURITY.md)",
        });
    }

    found
}

/// V11 and V12 — the database rules.
///
/// Extracted so [`validate`] stays inside the workspace's function-length lint. The split is along
/// the subsystem boundary rather than an arbitrary line count, so the next rule has an obvious home.
fn database_violations(config: &PlatformConfig) -> Vec<Violation> {
    let mut found = Vec::new();

    // V11 — the pool bounds. A pool larger than the server's own `max_connections` divided by the
    // number of roles is an outage the first time all three scale out together, and an acquire
    // timeout at or above the public request timeout hides pool saturation behind a request
    // timeout, which points the investigation at the wrong subsystem.
    let Some(database) = &config.database else {
        return found;
    };

    {
        if !(1..=100).contains(&database.max_connections) {
            found.push(Violation {
                key: "database.max_connections",
                env_var: "RATATOSKR__DATABASE__MAX_CONNECTIONS",
                rule: "must be 1..=100",
            });
        }

        if !(1..=30).contains(&database.acquire_timeout_seconds) {
            found.push(Violation {
                key: "database.acquire_timeout_seconds",
                env_var: "RATATOSKR__DATABASE__ACQUIRE_TIMEOUT_SECONDS",
                rule: "must be 1..=30",
            });
        }

        // V12 — the scheme. `postgres://` and `postgresql://` are the two sqlx accepts; anything
        // else fails at connect time, which is after the process has already reported itself
        // started. A configuration error must be a startup error.
        let url = database.url.expose_secret();
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            found.push(Violation {
                key: "database.url",
                env_var: "RATATOSKR__DATABASE__URL",
                rule: "must be a postgres:// or postgresql:// URL",
            });
        }
    }

    found
}

/// The V1 message for a role that must never open a public listener.
const fn forbidden_public_listener(role: RuntimeRole) -> &'static str {
    match role {
        // The first two are unreachable while both roles may listen publicly; kept exhaustive so a
        // new role must state its own rule text rather than inherit somebody else's.
        RuntimeRole::Edge => "the edge role must not open a public listener (ARCHITECTURE.md S18)",
        RuntimeRole::Ingest => {
            "the ingest role must not open a public listener (ARCHITECTURE.md S9)"
        }
        RuntimeRole::Scheduler => {
            "the scheduler role must not open a public listener (ARCHITECTURE.md S18)"
        }
    }
}

/// V1's other half: the roles that MUST listen publicly, and why each of them must.
const fn required_public_listener(role: RuntimeRole) -> &'static str {
    match role {
        RuntimeRole::Edge => "the edge role requires a public listener (ARCHITECTURE.md S18)",
        RuntimeRole::Ingest => {
            "the ingest role requires a public listener; a webhook source reaches it from outside \
             (ARCHITECTURE.md S9)"
        }
        // Unreachable: the scheduler may not have one, so it can never be missing one.
        RuntimeRole::Scheduler => {
            "the scheduler role must not open a public listener (ARCHITECTURE.md S18)"
        }
    }
}

/// `^[a-z0-9-]{1,64}$`, spelled without a regular-expression dependency.
fn is_header_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The operator-facing report for a set of violations. One block per problem, stable order, no
/// supplied values.
pub(crate) fn report_invalid(role: RuntimeRole, violations: &[Violation]) -> String {
    let plural = if violations.len() == 1 { "" } else { "s" };
    let mut out = format!(
        "{}: refusing to start; {} configuration problem{plural}.\n\n",
        role.binary_name(),
        violations.len(),
    );
    for violation in violations {
        let _ = writeln!(
            out,
            "  {}\n      {}\n      {}\n",
            violation.key, violation.env_var, violation.rule
        );
    }
    push_footer(&mut out, role);
    out
}

/// The operator-facing report for an extraction failure.
///
/// figment's message is deliberately NOT interpolated: it can quote the supplied value, and a
/// configuration report that echoes a value can echo a secret. Only keys are named.
pub(crate) fn report_unreadable(role: RuntimeRole, error: &figment::Error) -> String {
    let mut out = format!(
        "{}: refusing to start; the configuration could not be read.\n\n",
        role.binary_name(),
    );
    for problem in error.clone() {
        let key = key_of(&problem);
        let _ = writeln!(
            out,
            "  {key}\n      {}\n      {}\n",
            env_var_of(&key),
            reason_of(&problem),
        );
    }
    push_footer(&mut out, role);
    out
}

/// The two closing lines every report ends with.
fn push_footer(out: &mut String, role: RuntimeRole) {
    let _ = write!(
        out,
        "Supplied values are never echoed.\nValidate without starting: {} check-config\n",
        role.binary_name(),
    );
}

/// The dotted key an extraction failure is about; keys are safe to print, values are not.
fn key_of(error: &figment::Error) -> String {
    let path = error.path.join(".");
    match &error.kind {
        // figment reports a missing member under its PARENT's path, so the path alone names a key
        // the operator supplied correctly and an environment variable that cannot set the missing
        // field. Appending the member's own name is what makes the block actionable:
        // `telemetry.otlp.endpoint` / `RATATOSKR__TELEMETRY__OTLP__ENDPOINT`, not `telemetry.otlp`.
        Kind::MissingField(name) if path.is_empty() => name.to_string(),
        Kind::MissingField(name) => format!("{path}.{name}"),
        _ if !path.is_empty() => path,
        Kind::UnknownField(name, _) => name.clone(),
        _ => "(the provider did not report a key)".to_owned(),
    }
}

/// The environment variable a dotted key is set by.
fn env_var_of(key: &str) -> String {
    format!("RATATOSKR__{}", key.replace('.', "__").to_uppercase())
}

/// What went wrong, in terms that never quote the supplied value.
fn reason_of(error: &figment::Error) -> &'static str {
    match &error.kind {
        Kind::UnknownField(_, _) => "is not a configuration key of this process",
        Kind::MissingField(_) => "is required and was not supplied",
        _ => "could not be read as the type of this field",
    }
}
