//! The profile reader and renderer against fixtures and a real `nats-server -t`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::path::PathBuf;
use std::process::Command;

use nkeys::KeyPair;
use platform_nats_profile::{parse, render};

const ALPHA: &str = "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_ALPHA_XXXX";
const BETA: &str = "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_BETA_GAMMA_XX";

/// Two stanzas, comments (one holding a quote and a brace), and a deny list on the first only.
fn fixture() -> String {
    format!(
        r#"
# A comment with a "quote and a {{ brace.
port: 4222
authorization {{
    users: [
        {{
            # nkey: UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_COMMENTED_XX is only a comment
            nkey: {ALPHA}
            permissions: {{
                publish: {{
                    allow: ["cmd.>", "$JS.API.>"]
                    deny: ["$JS.API.STREAM.DELETE.>"]   # trailing comment
                }}
                subscribe: {{ allow: ["_INBOX.>"] }}
            }}
        }},
        {{
            nkey: {BETA}
            permissions: {{
                publish: {{ allow: [
                    "evt.one",
                    "evt.two",
                ] }}
                subscribe: {{ allow: ["_INBOX.>", "evt.three"] }}
            }}
        }}
    ]
}}
"#
    )
}

fn scratch_dir(label: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/nats-profile-fixtures")
        .join(format!("{label}-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&directory).expect("the scratch directory");
    directory
}

/// `nats-server -t -c <file>` in a container, the same image the deployment and CI run.
fn nats_server_test(conf: &str) -> std::process::Output {
    let directory = scratch_dir("dash-t");
    std::fs::write(directory.join("nats.conf"), conf).expect("the configuration file");
    let mount = format!("{}:/etc/nats-test:ro", directory.display());
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--user",
            "root",
            "--volume",
            &mount,
            "nats:2-alpine",
            "-t",
            "-c",
        ])
        .arg("/etc/nats-test/nats.conf")
        .output()
        .expect("docker must run nats-server -t");
    let _ = std::fs::remove_dir_all(&directory);
    output
}

#[test]
fn parse_reads_every_stanza_of_a_fixture_conf() {
    let identities = parse(&fixture()).expect("the fixture parses");
    assert_eq!(
        identities.len(),
        2,
        "the commented-out nkey is not a stanza"
    );

    let alpha = &identities[0];
    assert_eq!(alpha.name, "ALPHA");
    assert_eq!(alpha.placeholder, ALPHA);
    assert_eq!(alpha.publish_allow, ["cmd.>", "$JS.API.>"]);
    assert_eq!(alpha.publish_deny, ["$JS.API.STREAM.DELETE.>"]);
    assert_eq!(alpha.subscribe_allow, ["_INBOX.>"]);

    let beta = &identities[1];
    assert_eq!(beta.name, "BETA_GAMMA");
    assert_eq!(beta.placeholder, BETA);
    assert_eq!(beta.publish_allow, ["evt.one", "evt.two"]);
    assert!(beta.publish_deny.is_empty());
    assert_eq!(beta.subscribe_allow, ["_INBOX.>", "evt.three"]);
}

#[test]
fn render_replaces_every_placeholder_with_a_valid_nkey_and_returns_matching_seeds() {
    let rendered = render(&fixture()).expect("the fixture renders");
    assert!(
        !rendered
            .conf
            .contains("UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_ALPHA"),
        "a placeholder remains"
    );
    assert!(!rendered.conf.contains(ALPHA) && !rendered.conf.contains(BETA));
    assert_eq!(rendered.seeds.len(), 2);
    assert_eq!(
        rendered
            .seeds
            .iter()
            .map(|seed| seed.name.as_str())
            .collect::<Vec<_>>(),
        ["ALPHA", "BETA_GAMMA"]
    );
    for seed in &rendered.seeds {
        let pair = KeyPair::from_seed(&seed.seed).expect("the seed decodes");
        assert_eq!(pair.public_key(), seed.public_key, "{}", seed.name);
        assert!(seed.public_key.starts_with('U'), "a user key");
        assert!(
            rendered
                .conf
                .contains(&format!("nkey: {}", seed.public_key)),
            "{} is not substituted into the configuration",
            seed.name
        );
    }
    // Everything but the two nkey tokens is untouched.
    let restored = rendered
        .seeds
        .iter()
        .zip([ALPHA, BETA])
        .fold(rendered.conf.clone(), |text, (seed, placeholder)| {
            text.replace(&seed.public_key, placeholder)
        });
    assert_eq!(restored, fixture());

    let duplicated = fixture().replace(BETA, ALPHA);
    assert!(
        render(&duplicated).is_err(),
        "a duplicate placeholder is refused"
    );
}

#[test]
fn render_output_is_accepted_by_nats_server_dash_t() {
    let rendered = render(&fixture()).expect("the fixture renders");
    let accepted = nats_server_test(&rendered.conf);
    assert!(
        accepted.status.success(),
        "nats-server -t refused the rendered configuration: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    let refused = nats_server_test(&fixture());
    assert!(
        !refused.status.success(),
        "nats-server -t must refuse the unrendered placeholders, otherwise this tool has no reason to exist"
    );
}
