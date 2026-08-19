//! Every deployable boots on the configuration `DEVELOPMENT.md` documents — test B-1.
//!
//! This is the only test that runs the shipped binaries as processes. It exists so that the
//! "Local run" block of `DEVELOPMENT.md` cannot rot: each command there is executed here, the
//! admin plane is probed over a real socket, and the documented `SIGTERM` shutdown is asserted to
//! exit `0`.
//!
//! It lives in `services/edge` because that is the one package cargo builds all three binaries
//! for; `cargo test --workspace` is the documented command and it builds the other two alongside.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::io::{Read, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long a binary may take to answer `/health/ready` with `200`. Generous: a loaded CI runner
/// starting a cold process is the slow case, and the cost of a too-short timeout is a flake.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Between readiness polls.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// B-1. Each role starts on its documented environment, reports ready on its documented admin
/// port, and exits `0` on `SIGTERM` after the drain.
///
/// One test rather than three so the roles run sequentially: they bind fixed ports, which is the
/// point — those ports are the ones `DEVELOPMENT.md` tells an operator to use.
#[test]
fn each_role_boots_on_its_documented_configuration_and_reports_ready() {
    // `DEVELOPMENT.md`, "Local run": edge, with a public listener AND a database. Since milestone 5
    // every route edge serves reads or writes one, so it refuses to start without it — a process
    // that reported itself ready and then failed every request would be worse than one that did not
    // start. The refusal itself is asserted separately below.
    boots(
        "ratatoskr-edge",
        &[
            ("RATATOSKR__PUBLIC__BIND", "127.0.0.1:8080"),
            ("RATATOSKR__ADMIN__BIND", "127.0.0.1:9464"),
            ("RATATOSKR__TELEMETRY__LOG_FORMAT", "pretty"),
            ("RATATOSKR__DATABASE__URL", &database_url()),
        ],
        9464,
    );

    // Ingest, since milestone 7: a public listener of its own and a database it does not migrate.
    // It runs AFTER edge on purpose — edge owns the migrations, so this order is the ownership
    // relation, executed. Reversing the two makes ingest refuse to start, which is the behaviour
    // `ingest_refuses_to_start_against_an_unmigrated_database` asserts deliberately.
    boots(
        "ratatoskr-ingest",
        &[("RATATOSKR__DATABASE__URL", &database_url())],
        9465,
    );

    // `DEVELOPMENT.md`, "Local run": the scheduler, on defaults alone, no environment at all. It is
    // the one role that still needs none, and the one that never binds a public listener.
    boots("ratatoskr-scheduler", &[], 9466);
}

/// Where edge's database is. Matches `compose.yaml`, so `docker compose up -d` then `cargo test`
/// works with no further setup.
#[expect(
    clippy::disallowed_methods,
    reason = "the workspace bans direct environment reads so configuration has one loader. This is \
              a test binary choosing which database to point a child process at."
)]
fn database_url() -> String {
    std::env::var("PLATFORM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://platform:platform@127.0.0.1:5432/platform".to_owned())
}

/// B-4. Edge refuses to start without a database, loudly and at startup.
///
/// The alternative — starting and failing every request — is the failure mode `ARCHITECTURE.md` S16
/// calls a dependency that is unavailable, and reporting ready in that state makes a rollout
/// succeed into an outage.
#[test]
fn edge_refuses_to_start_without_a_database() {
    let path = built_binary("ratatoskr-edge");
    let output = Command::new(&path)
        .env("RATATOSKR__ADMIN__BIND", "127.0.0.1:9467")
        .env("RATATOSKR__PUBLIC__BIND", "127.0.0.1:8090")
        .env_remove("RATATOSKR__DATABASE__URL")
        .output()
        .unwrap_or_else(|error| panic!("{} could not be spawned: {error}", path.display()));

    assert!(
        !output.status.success(),
        "edge must not start without a database"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("RATATOSKR__DATABASE__URL"),
        "the refusal must name the variable that is missing\n{text}"
    );
}

/// B-5. Ingest refuses to start without a database, for the same reason edge does: every signal it
/// accepts is resolved against one, so a process that started anyway would report itself ready and
/// then fail every delivery a source made to it.
#[test]
fn ingest_refuses_to_start_without_a_database() {
    let text = refuses_to_start(
        "ratatoskr-ingest",
        &[
            ("RATATOSKR__ADMIN__BIND", "127.0.0.1:9468"),
            ("RATATOSKR__PUBLIC__BIND", "127.0.0.1:8091"),
        ],
    );
    assert!(
        text.contains("RATATOSKR__DATABASE__URL"),
        "the refusal must name the variable that is missing\n{text}"
    );
}

/// B-6. Ingest refuses to start against a database nobody migrated, and says who does.
///
/// `docs/ARCHITECTURE.md` S18 gives ingest a least-privilege database role, and a role that may
/// create a schema is not least-privilege — so ingest applies no migrations and `ratatoskr-edge`
/// owns them. Without this check the failure would arrive as a Postgres error on the first inbound
/// signal, hours later, in a log nobody is reading.
///
/// The `postgres` maintenance database is the target because it is guaranteed to exist on the same
/// server, is reachable with the same credential, and has never had a Platform migration applied to
/// it. No fixture, no cleanup, and nothing to leave behind.
#[test]
fn ingest_refuses_to_start_against_an_unmigrated_database() {
    let unmigrated = maintenance_database_url();
    let text = refuses_to_start(
        "ratatoskr-ingest",
        &[
            ("RATATOSKR__ADMIN__BIND", "127.0.0.1:9469"),
            ("RATATOSKR__PUBLIC__BIND", "127.0.0.1:8092"),
            ("RATATOSKR__DATABASE__URL", &unmigrated),
        ],
    );
    assert!(
        text.contains("platform_ingest"),
        "the refusal must name the schema that is absent\n{text}"
    );
    assert!(
        text.contains("ratatoskr-edge"),
        "the refusal must name the process that applies the migrations\n{text}"
    );
}

/// The `postgres` maintenance database on the same server as [`database_url`].
///
/// Built by replacing the path rather than by a second environment variable, so pointing the suite
/// at another server moves both.
fn maintenance_database_url() -> String {
    let url = database_url();
    match url.rfind('/') {
        Some(cut) => format!("{}/postgres", &url[..cut]),
        None => url,
    }
}

/// Runs `binary` to completion with `env`, asserts it failed, and returns both its streams.
fn refuses_to_start(binary: &str, env: &[(&str, &str)]) -> String {
    let path = built_binary(binary);
    // Removed first, then `env` is applied over it: a caller that supplies a database gets theirs,
    // and one that does not gets none even if the developer running the suite exported one.
    let output = Command::new(&path)
        .env_remove("RATATOSKR__DATABASE__URL")
        .envs(env.iter().copied())
        .output()
        .unwrap_or_else(|error| panic!("{} could not be spawned: {error}", path.display()));

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{binary} must not start\n{text}");
    text
}

/// Spawns `binary` with `env`, waits for readiness on `admin_port`, sends `SIGTERM`, and asserts a
/// clean exit. Both streams are reported with every failure, and both are needed: every log record
/// goes to **stdout** (§5.1), while stderr carries only what is written before a subscriber exists
/// — the configuration report and the reason telemetry or a listener refused to start.
fn boots(binary: &str, env: &[(&str, &str)], admin_port: u16) {
    let path = built_binary(binary);
    let mut child = Command::new(&path)
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{} could not be spawned: {error}", path.display()));

    let ready = wait_until_ready(admin_port);
    terminate(&child);
    let status = child
        .wait()
        .unwrap_or_else(|error| panic!("waiting for {binary} failed: {error}"));
    // The `pretty` format writes ANSI colour sequences between a field name and its value, so the
    // text has to be stripped before any of it can be matched.
    let out = strip_ansi(&drain(child.stdout.take()));
    let log = format!(
        "--- stdout ---\n{out}--- stderr ---\n{}",
        drain(child.stderr.take())
    );

    assert!(
        ready,
        "{binary} never answered 200 on http://127.0.0.1:{admin_port}/health/ready\n{log}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "{binary} did not exit 0 after SIGTERM ({status})\n{log}"
    );
    // The two spans §5.3 requires, on the stream they are actually written to.
    assert!(
        out.contains("startup complete"),
        "{binary} logged no startup line\n{log}"
    );
    assert!(
        out.contains("\"graceful\":true") || out.contains("graceful: true"),
        "{binary} logged no graceful shutdown\n{log}"
    );
}

/// `text` without ANSI control sequences.
fn strip_ansi(text: &str) -> String {
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

/// B-1b. `check-config` is the documented init-container and CI pre-flight, so its exit codes are
/// an operational contract. Nothing else runs the subcommand: deleting the branch from all three
/// `main`s leaves every other test green.
#[test]
fn check_config_exits_zero_on_a_valid_configuration_and_78_on_an_invalid_one() {
    // All three, because the subcommand is wired into each `main` separately.
    for binary in ["ratatoskr-edge", "ratatoskr-ingest", "ratatoskr-scheduler"] {
        let valid = Command::new(built_binary(binary))
            .arg("check-config")
            .output()
            .expect("check-config must run");
        assert_eq!(
            valid.status.code(),
            Some(0),
            "{binary}: the defaults must validate: {}",
            String::from_utf8_lossy(&valid.stderr)
        );
    }

    let invalid = Command::new(built_binary("ratatoskr-scheduler"))
        .arg("check-config")
        .env("RATATOSKR__PUBLIC__BIND", "0.0.0.0:8080")
        .output()
        .expect("check-config must run");
    let report = String::from_utf8_lossy(&invalid.stderr);
    assert_eq!(invalid.status.code(), Some(78), "EX_CONFIG\n{report}");
    assert!(report.contains("public.bind"), "{report}");
    assert!(
        !report.contains("0.0.0.0:8080"),
        "the report echoed the supplied value: {report}"
    );
}

/// B-1c. Exit `1`, the third row of the §3.6 table: a listener that could not bind is a runtime
/// startup failure, and a restart-loop dashboard distinguishes it from `78` by this code alone.
#[test]
fn a_listener_that_cannot_bind_exits_one() {
    // Held open for the child's whole life; a second listener on the same port is `EADDRINUSE`.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("a port must be available");
    let port = taken.local_addr().expect("the port is known").port();

    let refused = Command::new(built_binary("ratatoskr-scheduler"))
        .env("RATATOSKR__ADMIN__BIND", format!("127.0.0.1:{port}"))
        .output()
        .expect("the binary must run");

    assert_eq!(
        refused.status.code(),
        Some(1),
        "a bind failure is exit 1, not 78 and not 0\n{}{}",
        strip_ansi(&String::from_utf8_lossy(&refused.stdout)),
        String::from_utf8_lossy(&refused.stderr),
    );
    assert!(
        strip_ansi(&String::from_utf8_lossy(&refused.stdout))
            .contains("the admin listener could not bind"),
        "the operator was not told which listener failed",
    );
}

/// The path of a workspace binary, resolved beside this package's own one.
///
/// `CARGO_BIN_EXE_*` is set only for the binaries of the package under test, so the other two are
/// found by name in the same directory. Only a build puts them there: `cargo build --workspace`
/// does, and `cargo test --workspace` does NOT — it builds the binary of the package whose tests it
/// is running, never a sibling package's plain binary. The gate therefore runs `cargo build
/// --workspace --locked` before `cargo test`, and DEVELOPMENT.md records why.
fn built_binary(binary: &str) -> PathBuf {
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
fn wait_until_ready(admin_port: u16) -> bool {
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
fn probe_ready(admin_port: u16) -> Option<String> {
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
fn terminate(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("kill(1) is available on any unix host");
    assert!(status.success(), "SIGTERM could not be delivered: {status}");
}

/// Everything the child wrote to one stream. Read after `wait`, so the pipe is complete; a startup
/// and shutdown log is a few kilobytes and cannot fill it.
fn drain(stream: Option<impl Read>) -> String {
    let mut text = String::new();
    if let Some(mut stream) = stream {
        let _ = stream.read_to_string(&mut text);
    }
    text
}
