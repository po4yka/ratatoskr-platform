//! XR-021 R2-04 rule 1, without Docker: the deployed broker file raises `max_payload`.
//!
//! Archive and document facts are published as inline envelopes, so the 1 MiB server default
//! refuses a large item on every attempt. The real-broker twin is
//! `deployed_nats_config.rs::x_identity_publishes_a_four_mebibyte_envelope`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

const CONF: &str = include_str!("../../../deploy/nats/ratatoskr.conf");

/// The file with every `#` comment removed, so a value that appears only in prose does not count.
fn without_comments(conf: &str) -> String {
    conf.lines()
        .map(|line| line.split_once('#').map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn deployed_conf_sets_max_payload() {
    let code = without_comments(CONF);
    // Top level only: a line with no indentation. NATS reads `8MB` as 8388608 bytes.
    let setting: Vec<&str> = code
        .lines()
        .filter(|line| line.trim_start().starts_with("max_payload"))
        .collect();
    assert_eq!(
        setting.len(),
        1,
        "exactly one max_payload setting expected: {setting:?}"
    );
    let line = setting.first().expect("one setting");
    assert!(
        !line.starts_with(char::is_whitespace),
        "max_payload must be a top-level setting, not nested in a block: {line:?}"
    );
    let value = line
        .split_once(':')
        .map(|(_, value)| value.trim())
        .expect("max_payload: <value>");
    assert_eq!(value, "8MB");
}
