//! The bus alert rule file agrees with the metric names (XR-021 CONTRACTS.md R5-13 rule 2).
//!
//! A text test, because no rule engine runs in CI: the file must parse as YAML, every alert must be
//! complete, and every series an expression names must be one that `platform_telemetry::metrics`
//! defines or one of the producer-side names the file documents. A rename in `metrics.rs` that left
//! a rule watching a series nobody exports would be an alert that can never fire.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use platform_telemetry::metrics;

/// The producer-side series the file documents. The five deletion producers export them about
/// their own outbox (R5-11), so they are not constants of this repository's `metrics.rs`.
const PRODUCER_SUFFIXES: [&str; 1] = ["_oldest_unacknowledged_tombstone_age_seconds"];

/// The six alerts of R5-13 (2).
const EXPECTED: [&str; 6] = [
    "RatatoskrKnowledgeConsumerLag",
    "RatatoskrKnowledgeConsumerNearRetention",
    "RatatoskrKnowledgeConsumerBacklog",
    "RatatoskrEventsStreamNearCapacity",
    "RatatoskrConsumerRedeliveryStorm",
    "RatatoskrUnacknowledgedTombstones",
];

/// `PromQL` words that look like series names and are not.
const KEYWORDS: [&str; 18] = [
    "by",
    "without",
    "on",
    "ignoring",
    "group_left",
    "group_right",
    "and",
    "or",
    "unless",
    "bool",
    "max",
    "min",
    "sum",
    "avg",
    "count",
    "increase",
    "rate",
    "offset",
];

fn path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/monitoring/ratatoskr-bus-alerts.rules.yml")
}

fn text() -> String {
    std::fs::read_to_string(path()).unwrap_or_else(|error| {
        panic!(
            "deploy/monitoring/ratatoskr-bus-alerts.rules.yml must exist and be readable: {error}"
        )
    })
}

fn rules() -> Vec<serde_json::Value> {
    let document: serde_json::Value =
        serde_norway::from_str(&text()).expect("the rule file is YAML");
    document["groups"]
        .as_array()
        .expect("a groups list")
        .iter()
        .flat_map(|group| group["rules"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// The series names in a `PromQL` expression, and the `__name__` regexes it matches on.
fn names_in(expr: &str) -> (Vec<String>, Vec<String>) {
    let mut series = Vec::new();
    let mut regexes = Vec::new();
    // `{__name__=~"..."}` selects by a pattern instead of a name.
    let mut rest = expr;
    while let Some((_, after)) = rest.split_once("__name__=~\"") {
        let Some((regex, tail)) = after.split_once('"') else {
            panic!("an unclosed string in {expr}");
        };
        regexes.push(regex.to_owned());
        rest = tail;
    }
    let mut inside = 0_u32;
    // After `by` or `without` comes a list of label names, which are not series.
    let mut grouping = false;
    let mut in_labels = false;
    let mut token = String::new();
    let mut keep = |token: &mut String, grouping: &mut bool| {
        let word = std::mem::take(token);
        if word.is_empty() {
            return;
        }
        *grouping = matches!(word.as_str(), "by" | "without");
        if !word.starts_with(|c: char| c.is_ascii_digit()) && !KEYWORDS.contains(&word.as_str()) {
            series.push(word);
        }
    };
    for character in expr.chars() {
        match character {
            ')' if in_labels => in_labels = false,
            _ if in_labels => {}
            '(' if grouping => {
                in_labels = true;
                grouping = false;
            }
            '{' | '[' => {
                keep(&mut token, &mut grouping);
                inside += 1;
            }
            '}' | ']' => inside = inside.saturating_sub(1),
            _ if inside > 0 => {}
            c if c.is_ascii_alphanumeric() || c == '_' || c == ':' => token.push(c),
            c if c.is_whitespace() => keep(&mut token, &mut grouping),
            _ => {
                keep(&mut token, &mut grouping);
                grouping = false;
            }
        }
    }
    keep(&mut token, &mut grouping);
    (series, regexes)
}

#[test]
fn the_rule_file_exists_and_holds_the_six_alerts() {
    let alerts: Vec<String> = rules()
        .iter()
        .filter_map(|rule| rule["alert"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(alerts, EXPECTED);
}

#[test]
fn every_alert_is_complete() {
    for rule in rules() {
        let name = rule["alert"].as_str().expect("an alert name");
        for member in ["expr", "for"] {
            assert!(
                rule[member].as_str().is_some_and(|value| !value.is_empty()),
                "{name} has no {member}"
            );
        }
        assert!(
            matches!(
                rule["labels"]["severity"].as_str(),
                Some("warning" | "critical")
            ),
            "{name} has no severity of warning or critical"
        );
        assert!(
            rule["annotations"]["summary"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "{name} has no summary"
        );
    }
}

#[test]
fn every_series_in_an_expression_is_exported_or_documented() {
    let file = text();
    for suffix in PRODUCER_SUFFIXES {
        assert!(
            file.contains(&format!("*{suffix}")),
            "the file does not document the producer-side name *{suffix}"
        );
    }
    for rule in rules() {
        let name = rule["alert"].as_str().expect("an alert name");
        let expr = rule["expr"].as_str().expect("an expression");
        let (series, regexes) = names_in(expr);
        for series in &series {
            assert!(
                metrics::ALL.contains(&series.as_str()),
                "{name} uses `{series}`, which platform_telemetry::metrics does not define"
            );
        }
        for regex in &regexes {
            assert!(
                PRODUCER_SUFFIXES
                    .iter()
                    .any(|suffix| regex.strip_prefix(".*") == Some(suffix)),
                "{name} selects by the pattern `{regex}`, which is not a documented producer name"
            );
        }
        assert!(
            !series.is_empty() || !regexes.is_empty(),
            "{name} names no series at all: {expr}"
        );
    }
}

/// The extraction itself is checked on the real expressions, so a lexer that silently found nothing
/// cannot make the test above pass.
#[test]
fn the_extraction_finds_the_series_of_the_real_expressions() {
    let by_alert: std::collections::BTreeMap<String, String> = rules()
        .iter()
        .map(|rule| {
            (
                rule["alert"].as_str().expect("an alert").to_owned(),
                rule["expr"].as_str().expect("an expression").to_owned(),
            )
        })
        .collect();
    let series = |alert: &str| names_in(by_alert.get(alert).expect("the alert exists")).0;
    assert_eq!(
        series("RatatoskrKnowledgeConsumerLag"),
        ["platform_bus_consumer_oldest_unacked_age_seconds"]
    );
    assert_eq!(
        series("RatatoskrEventsStreamNearCapacity"),
        ["platform_bus_stream_bytes", "platform_bus_stream_max_bytes"]
    );
    assert_eq!(
        series("RatatoskrConsumerRedeliveryStorm"),
        ["platform_bus_consumer_redelivered"]
    );
    let (names, regexes) = names_in(
        by_alert
            .get("RatatoskrUnacknowledgedTombstones")
            .expect("the alert exists"),
    );
    assert!(names.is_empty(), "{names:?}");
    assert_eq!(regexes, [".*_oldest_unacknowledged_tombstone_age_seconds"]);
}
