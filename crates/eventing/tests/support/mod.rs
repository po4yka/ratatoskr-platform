//! Shared helpers for the tests that start a disposable `nats:2-alpine` container with a generated
//! configuration and connect to it as an nkey identity.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions and disposable resource cleanup in a test binary"
)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tokio::time::sleep;
use uuid::Uuid;

/// How long a request waits for an answer. A refused publish is never answered, so this is also how
/// long a refusal takes to observe.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// One disposable broker container and the directory holding its configuration.
#[derive(Debug)]
pub(crate) struct Container {
    name: String,
    directory: PathBuf,
    /// The URL of the published client port on the host loopback.
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
            url: format!("nats://127.0.0.1:{port}"),
        }
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Connect as the identity that owns `seed`, retrying while the server starts.
pub(crate) async fn connect(url: &str, seed: &str) -> async_nats::Client {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match async_nats::ConnectOptions::with_nkey(seed.to_owned())
            .request_timeout(Some(REQUEST_TIMEOUT))
            .connect(url)
            .await
        {
            Ok(client) => return client,
            Err(_) if tokio::time::Instant::now() < deadline => {
                sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("the disposable NATS identity did not connect: {error}"),
        }
    }
}
