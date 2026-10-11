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
        let now = jiff::Timestamp::now();
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
