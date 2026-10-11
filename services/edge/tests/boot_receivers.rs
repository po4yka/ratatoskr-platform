//! Edge booted with the SHIPPED configuration, on a host where nothing else runs (XR-021 CONTRACTS.md
//! R3-13 and R3-12): the optional archive receivers do not hold it out of rotation, and the Telegram
//! assertion key in the example is checked at startup. Split from `boot.rs` to keep both under the
//! repository's file-size cap; the helpers are in `support`.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::io::{Read, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use platform_persistence::test_support::TestDatabase;

mod boot_support;

use boot_support::{
    built_binary, bus_url, drain, probe_ready, strip_ansi, terminate, wait_until_ready,
};

/// Serializes the tests that start edge on the fixed ports of this file.
static EDGE_BOOT_LOCK: Mutex<()> = Mutex::new(());

/// The bearer credential `seed_browser_session` installs.
const BOOT_CREDENTIAL: &str = "boot-capabilities-credential-0000";

/// A user with a live browser session that authenticates with [`BOOT_CREDENTIAL`] on Edge.
fn seed_browser_session(runtime: &tokio::runtime::Runtime, database: &TestDatabase) {
    runtime.block_on(async {
        let now = jiff::Timestamp::now(); // wall-clock: the spawned edge binary checks session expiry against its own clock
        let user = platform_identity::user::create_user(database.pool(), now)
            .await
            .expect("a user");
        platform_identity::session::create_session(
            database.pool(),
            &platform_identity::NewSession {
                user_id: user.user_id,
                kind: platform_identity::SessionKind::Browser,
                device_id: None,
                audience: "edge",
                token: Some(platform_public_api::auth::credential_digest(
                    BOOT_CREDENTIAL,
                )),
                issued_at: now,
                expires_at: now + jiff::SignedDuration::from_hours(1),
            },
        )
        .await
        .expect("a session");
    });
}

/// One `GET` written onto a raw socket to the public listener, with a bearer credential. Returns the
/// status line and the body.
fn public_get(port: u16, path: &str, credential: &str) -> Option<(String, String)> {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    socket
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {credential}\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .ok()?;
    let mut response = String::new();
    socket.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status = head.lines().next()?.to_owned();
    Some((status, body.to_owned()))
}

/// B-13 (XR-021 R3-13). Edge booted with the SHIPPED route table, on a host where none of the
/// domain services or archive receivers is running, is ready: the receivers are optional, so a start
/// order that brings Edge up first, or a partial deployment, must not hold it out of rotation. The
/// capability document says so truthfully: every configured service is stale and carries no
/// document.
#[test]
fn edge_with_the_shipped_route_table_is_ready_without_any_receiver() {
    let _edge_boot = EDGE_BOOT_LOCK.lock().expect("the edge boot lock");
    let example = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd/edge.conf.example"),
    )
    .expect("the Edge example");
    let shipped: Vec<(String, String)> = example
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("RATATOSKR__GATEWAY__ROUTES__")
                || line.starts_with("RATATOSKR__PUBLIC__REQUEST_TIMEOUT_SECONDS=")
                || line.starts_with("RATATOSKR__PUBLIC__MAX_BODY_BYTES=")
        })
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    assert!(shipped.len() >= 24, "the shipped route table: {shipped:?}");
    for port in 8091..=8098 {
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_err(),
            "port {port} must be free: this test needs every receiver absent"
        );
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a fixture runtime");
    let database = runtime
        .block_on(TestDatabase::create())
        .expect("a prepared database");
    seed_browser_session(&runtime, &database);
    let staging = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/archive-staging-boot")
        .join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir_all(&staging).expect("a staging root");

    let mut command = Command::new(built_binary("ratatoskr-edge"));
    command
        .envs(
            shipped
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .env("RATATOSKR__PUBLIC__BIND", "127.0.0.1:8101")
        .env("RATATOSKR__ADMIN__BIND", "127.0.0.1:9474")
        .env("RATATOSKR__ARCHIVE_STAGING__ROOT", &staging)
        .env("RATATOSKR__DATABASE__URL", database.url())
        .env("RATATOSKR__BUS__URL", bus_url())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("edge spawns");

    let ready = wait_until_ready(9474);
    let readiness = probe_ready(9474).unwrap_or_default();
    let capabilities = public_get(8101, "/v1/capabilities", BOOT_CREDENTIAL);
    terminate(&child);
    let status = child.wait().expect("waiting for edge");
    let log = format!(
        "{}{}",
        strip_ansi(&drain(child.stdout.take())),
        drain(child.stderr.take())
    );
    runtime
        .block_on(database.cleanup())
        .expect("the fixture database must drop");
    let _ = std::fs::remove_dir_all(&staging);

    assert!(
        ready,
        "edge was never ready with every receiver absent\n{readiness}\n{log}"
    );
    assert_eq!(status.code(), Some(0), "{log}");
    for failed in ["archive_chatgpt_receipt", "archive_claude_receipt"] {
        assert!(
            readiness.contains(failed),
            "the readiness body still reports {failed}: {readiness}"
        );
    }
    let (line, body) = capabilities.expect("the capability document is served");
    assert!(line.contains("200"), "{line}\n{body}");
    // No JSON parser is a dependency of this package, so the document is counted rather than
    // parsed: seven configured services, each stale and each without a document.
    assert_eq!(body.matches("\"service\":").count(), 7, "{body}");
    assert_eq!(body.matches("\"stale\":true").count(), 7, "{body}");
    assert_eq!(body.matches("\"document\":null").count(), 7, "{body}");
}

/// R3-12. The Edge example ships `RATATOSKR__IDENTITY__ASSERTION_KEY=CHANGE-ME` active. The loader
/// refuses the placeholder at startup, by naming the key, and accepts the RFC 8032 vector (the
/// public key of Telegram's known-answer seed), in the form the example documents.
#[test]
fn the_edge_example_assertion_key_is_checked_at_startup() {
    const VECTOR: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    let example = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd/edge.conf.example"),
    )
    .expect("the Edge example");
    let placeholder = example
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("RATATOSKR__IDENTITY__ASSERTION_KEY="))
        .expect("the example carries an active assertion key line");

    let refused = Command::new(built_binary("ratatoskr-edge"))
        .arg("check-config")
        .env("RATATOSKR__IDENTITY__ASSERTION_KEY", placeholder)
        .output()
        .expect("check-config must run");
    let report = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(78), "{report}");
    assert!(report.contains("identity.assertion_key"), "{report}");
    assert!(
        !report.contains(placeholder) || placeholder.is_empty(),
        "the report echoed the supplied value: {report}"
    );

    let accepted = Command::new(built_binary("ratatoskr-edge"))
        .arg("check-config")
        .env("RATATOSKR__IDENTITY__ASSERTION_KEY", VECTOR)
        .output()
        .expect("check-config must run");
    assert_eq!(
        accepted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

/// R3-12. Edge booted with a database and the RFC 8032 public key offers the Telegram Mini App.
/// Without the key `telegram.mini_app` is absent from the capability document (ADR-0011).
#[test]
fn an_edge_with_the_assertion_key_and_a_database_offers_the_telegram_mini_app() {
    const VECTOR: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    let _edge_boot = EDGE_BOOT_LOCK.lock().expect("the edge boot lock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a fixture runtime");
    let database = runtime
        .block_on(TestDatabase::create())
        .expect("a prepared database");
    seed_browser_session(&runtime, &database);

    let mut offered = Vec::new();
    for (public, admin, key) in [(8102, 9475, Some(VECTOR)), (8103, 9476, None)] {
        let mut command = Command::new(built_binary("ratatoskr-edge"));
        command
            .env("RATATOSKR__PUBLIC__BIND", format!("127.0.0.1:{public}"))
            .env("RATATOSKR__ADMIN__BIND", format!("127.0.0.1:{admin}"))
            .env("RATATOSKR__DATABASE__URL", database.url())
            .env("RATATOSKR__BUS__URL", bus_url())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(key) = key {
            command.env("RATATOSKR__IDENTITY__ASSERTION_KEY", key);
        }
        let mut child = command.spawn().expect("edge spawns");
        let ready = wait_until_ready(admin);
        let capabilities = public_get(public, "/v1/capabilities", BOOT_CREDENTIAL);
        terminate(&child);
        child.wait().expect("waiting for edge");
        assert!(ready, "edge never became ready on {public}");
        let (line, body) = capabilities.expect("the capability document is served");
        assert!(line.contains("200"), "{line}\n{body}");
        offered.push(body.contains("\"telegram.mini_app\""));
    }
    runtime
        .block_on(database.cleanup())
        .expect("the fixture database must drop");
    assert_eq!(
        offered,
        [true, false],
        "telegram.mini_app is offered exactly when the assertion key is configured"
    );
}
