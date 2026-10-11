//! Helpers shared by the Edge boot test binaries: where the broker is, the built binaries, and the
//! raw-socket probes of the admin plane. A client library would be the only dependency this package
//! has beyond the three the specification grants it, so the probes are written onto sockets.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::io::{Read, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long a binary may take to answer `/health/ready` with `200`. Generous: a loaded CI runner
/// starting a cold process is the slow case, and the cost of a too-short timeout is a flake.
pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Between readiness polls.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Where edge's bus is. Matches `compose.yaml`, like the database below.
#[expect(
    clippy::disallowed_methods,
    reason = "a test binary choosing which broker to point a child process at"
)]
pub(crate) fn bus_url() -> String {
    std::env::var("PLATFORM_TEST_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_owned())
}

/// `text` without ANSI control sequences.
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // A CSI sequence runs to its first alphabetic byte, which is `m` for every colour.
            for next in characters.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(character);
        }
    }
    out
}

/// The path of a workspace binary, resolved beside this package's own one.
///
/// `CARGO_BIN_EXE_*` is set only for the binaries of the package under test, so the other two are
/// found by name in the same directory. Only a build puts them there: `cargo build --workspace`
/// does, and `cargo test --workspace` does NOT — it builds the binary of the package whose tests it
/// is running, never a sibling package's plain binary. The gate therefore runs `cargo build
/// --workspace --locked` before `cargo test`, and DEVELOPMENT.md records why.
pub(crate) fn built_binary(binary: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_ratatoskr-edge")).with_file_name(binary);
    assert!(
        path.is_file(),
        "{} has not been built; run `cargo build --workspace` first (`cargo test` does not build \
         a sibling package's binary)",
        path.display()
    );
    path
}

/// Polls `/health/ready` until it answers `200` with `"state":"ready"`, or the timeout expires.
///
/// A `503` early on is expected and not a failure: readiness is `not_ready` between the admin
/// listener binding and `mark_startup_complete`.
pub(crate) fn wait_until_ready(admin_port: u16) -> bool {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(response) = probe_ready(admin_port)
            && response.starts_with("HTTP/1.1 200")
            && response.contains("\"state\":\"ready\"")
        {
            return true;
        }
        sleep(POLL_INTERVAL);
    }
    false
}

/// One `GET /health/ready` written onto a raw socket.
///
/// The admin plane speaks plain HTTP/1.1 and `Connection: close` makes the whole response readable
/// to EOF, so a client library would be the only dependency this package has beyond the three the
/// specification grants it.
pub(crate) fn probe_ready(admin_port: u16) -> Option<String> {
    let mut socket = TcpStream::connect(("127.0.0.1", admin_port)).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    socket
        .write_all(b"GET /health/ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    socket.read_to_string(&mut response).ok()?;
    Some(response)
}

/// Sends `SIGTERM`, the signal the shutdown sequence listens for.
///
/// `Child::kill` sends `SIGKILL`, which skips the drain entirely and never yields exit `0`, and
/// `libc::kill` is unavailable because the workspace forbids unsafe code. `kill(1)` is the
/// remaining route and it is the same command `DEVELOPMENT.md` documents.
pub(crate) fn terminate(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("kill(1) is available on any unix host");
    assert!(status.success(), "SIGTERM could not be delivered: {status}");
}

/// Everything the child wrote to one stream. Read after `wait`, so the pipe is complete; a startup
/// and shutdown log is a few kilobytes and cannot fill it.
pub(crate) fn drain(stream: Option<impl Read>) -> String {
    let mut text = String::new();
    if let Some(mut stream) = stream {
        let _ = stream.read_to_string(&mut text);
    }
    text
}
