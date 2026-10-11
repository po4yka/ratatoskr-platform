//! Shared helpers for the tests that start a disposable broker with a generated configuration and
//! connect to it as an nkey identity.
//!
//! The broker is a `nats:2-alpine` container, as `compose.yaml` runs it. On a machine without a
//! Docker daemon, `PLATFORM_TEST_NATS_SERVER` may name a native `nats-server` binary (2.10 or
//! later) instead: the same configuration then runs as a child process on a loopback port taken
//! from `PLATFORM_TEST_NATS_PORTS` (`first-last`, default `57623-57639`), with the `JetStream` store
//! moved into the fixture directory and the monitoring listener removed. CI uses the container.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::sleep;
use uuid::Uuid;

/// How long a request waits for an answer. A refused publish is never answered, so this is also how
/// long a refusal takes to observe.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The ports a native broker of this process holds. A test binary runs its tests in parallel, so
/// the claim must be process-wide.
static CLAIMED_PORTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

/// One disposable broker, a container or a native process, and the directory holding its
/// configuration.
#[derive(Debug)]
pub(crate) struct Container {
    name: String,
    directory: PathBuf,
    /// The native process, when `PLATFORM_TEST_NATS_SERVER` selected one, and the port it holds.
    native: Option<(Child, u16)>,
    /// The URL of the client port on the host loopback.
    pub(crate) url: String,
}

impl Container {
    /// Start the server with `conf` and publish its client port on a free host loopback port.
    ///
    /// `extra` is passed to `docker run` before the image, for example `["--user", "root"]`.
    pub(crate) fn start(label: &str, conf: &str, extra: &[&str]) -> Self {
        let name = format!("ratatoskr-platform-{label}-{}", Uuid::now_v7().simple());
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/nats-fixtures")
            .join(&name);
        std::fs::create_dir_all(&directory).expect("the disposable NATS directory");
        if let Some(binary) = native_server() {
            return Self::start_native(&binary, name, directory, conf);
        }
        std::fs::write(directory.join("nats.conf"), conf).expect("the NATS configuration");

        let mount = format!("{}:/etc/nats-fixture:ro", directory.display());
        let started = Command::new("docker")
            .args([
                "run",
                "--detach",
                "--name",
                &name,
                "--publish",
                "127.0.0.1::4222",
            ])
            .args(extra)
            .args([
                "--volume",
                &mount,
                "nats:2-alpine",
                "-c",
                "/etc/nats-fixture/nats.conf",
            ])
            .output()
            .expect("docker must start the disposable NATS server");
        assert!(
            started.status.success(),
            "disposable NATS failed to start: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        let port = Command::new("docker")
            .args(["port", &name, "4222/tcp"])
            .output()
            .expect("docker must report the disposable NATS port");
        if !port.status.success() {
            let logs = Command::new("docker")
                .args(["logs", &name])
                .output()
                .expect("docker must report why disposable NATS exited");
            let _ = Command::new("docker")
                .args(["rm", "--force", &name])
                .output();
            let _ = std::fs::remove_dir_all(&directory);
            panic!(
                "docker did not report the NATS port: {}{}",
                String::from_utf8_lossy(&port.stderr),
                String::from_utf8_lossy(&logs.stderr)
            );
        }
        let binding = String::from_utf8(port.stdout).expect("the port binding is UTF-8");
        let port = binding
            .lines()
            .next()
            .and_then(|line| line.trim().rsplit_once(':'))
            .map(|(_, port)| port.to_owned())
            .expect("the port binding has a port");

        Self {
            name,
            directory,
            native: None,
            url: format!("nats://127.0.0.1:{port}"),
        }
    }

    /// Run `binary` on `conf` as a child process. See the module comment for what changes.
    fn start_native(
        binary: &std::ffi::OsStr,
        name: String,
        directory: PathBuf,
        conf: &str,
    ) -> Self {
        let store = directory.join("store");
        let adapted = adapt_for_native(conf, &store);
        std::fs::write(directory.join("nats.conf"), adapted).expect("the NATS configuration");
        let port = claim_port();
        let log = std::fs::File::create(directory.join("server.log")).expect("the server log");
        let child = Command::new(binary)
            .args(["-c"])
            .arg(directory.join("nats.conf"))
            .args(["-a", "127.0.0.1", "-p", &port.to_string()])
            .stdout(Stdio::from(log.try_clone().expect("the server log handle")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("PLATFORM_TEST_NATS_SERVER must start");
        Self {
            name,
            directory,
            native: Some((child, port)),
            url: format!("nats://127.0.0.1:{port}"),
        }
    }
}

/// The configuration with its container-only settings replaced: the `JetStream` store moves into
/// the fixture directory and the monitoring listener (a fixed port) is dropped.
fn adapt_for_native(conf: &str, store: &std::path::Path) -> String {
    let mut adapted = String::with_capacity(conf.len());
    for line in conf.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("http:") || trimmed.starts_with("http_port:") {
            continue;
        }
        if let Some(at) = line.find("store_dir:") {
            let (head, tail) = line.split_at(at + "store_dir:".len());
            let tail = tail.trim_start();
            let end = tail
                .find(|c: char| c.is_whitespace() || c == '}')
                .unwrap_or(tail.len());
            adapted.push_str(head);
            adapted.push(' ');
            adapted.push_str(&store.display().to_string());
            adapted.push_str(tail.get(end..).unwrap_or_default());
        } else {
            adapted.push_str(line);
        }
        adapted.push('\n');
    }
    adapted
}

/// The native `nats-server` binary the environment selects, if any.
#[expect(
    clippy::disallowed_methods,
    reason = "a test helper choosing which broker to run; the workspace bans direct environment \
              reads so that production configuration has one loader"
)]
fn native_server() -> Option<std::ffi::OsString> {
    std::env::var_os("PLATFORM_TEST_NATS_SERVER")
}

/// The `first-last` port range the environment selects for native brokers, if any.
#[expect(
    clippy::disallowed_methods,
    reason = "a test helper choosing which ports to run on; see `native_server`"
)]
fn port_setting() -> Option<String> {
    std::env::var("PLATFORM_TEST_NATS_PORTS").ok()
}

/// The loopback port range a native broker may use.
fn port_range() -> std::ops::RangeInclusive<u16> {
    let text = port_setting().unwrap_or_else(|| "57623-57639".to_owned());
    let (first, last) = text
        .split_once('-')
        .expect("PLATFORM_TEST_NATS_PORTS is first-last");
    first.parse().expect("the first port")..=last.parse().expect("the last port")
}

/// Take a free port of the range, waiting while every one is held by another test.
fn claim_port() -> u16 {
    let deadline = std::time::Instant::now() + Duration::from_mins(3);
    loop {
        {
            let mut claimed = CLAIMED_PORTS.lock().expect("the port claims");
            for port in port_range() {
                if !claimed.contains(&port) && TcpListener::bind(("127.0.0.1", port)).is_ok() {
                    claimed.insert(port);
                    return port;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no free broker port in the configured range"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if let Some((mut child, port)) = self.native.take() {
            let _ = child.kill();
            let _ = child.wait();
            if let Ok(mut claimed) = CLAIMED_PORTS.lock() {
                claimed.remove(&port);
            }
        } else {
            let _ = Command::new("docker")
                .args(["rm", "--force", &self.name])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// The server errors a connection has received, in order. A refused publish or subscribe is only
/// visible here (and in the server log): the client call itself succeeds.
pub(crate) type ServerErrors = Arc<Mutex<Vec<String>>>;

/// Connect as the identity that owns `seed`, retrying while the server starts. `inbox_prefix` is the
/// connection's reply namespace (`None` keeps the library default `_INBOX`); the second value is the
/// server errors the connection receives.
pub(crate) async fn connect(
    url: &str,
    seed: &str,
    inbox_prefix: Option<&str>,
) -> (async_nats::Client, ServerErrors) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let errors = ServerErrors::default();
        let sink = Arc::clone(&errors);
        let mut options = async_nats::ConnectOptions::with_nkey(seed.to_owned())
            .request_timeout(Some(REQUEST_TIMEOUT))
            .event_callback(move |event| {
                let sink = Arc::clone(&sink);
                async move {
                    if let async_nats::Event::ServerError(error) = event {
                        sink.lock()
                            .expect("the server error sink")
                            .push(error.to_string());
                    }
                }
            });
        if let Some(prefix) = inbox_prefix {
            options = options.custom_inbox_prefix(prefix);
        }
        match options.connect(url).await {
            Ok(client) => return (client, errors),
            Err(_) if tokio::time::Instant::now() < deadline => {
                sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("the disposable NATS identity did not connect: {error}"),
        }
    }
}
