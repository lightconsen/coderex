//! Provider cassette support for the end-to-end suites.
//!
//! Every provider-backed suite used to be `#[ignore]`d because it needed a
//! real (or separately started mock) LLM. This module starts `llm-cassette`
//! once per test binary: in replay mode it serves the recordings committed
//! under `tests/cassettes/<suite>/`, and in record mode
//! (`CODEREX_CASSETTE_MODE=record`) it proxies to the endpoint named by
//! `CODEREX_CASSETTE_UPSTREAM` / `CODEREX_CASSETTE_API_KEY` and writes new
//! recordings.
//!
//! The child process is given an isolated `CLAUDE_CONFIG_DIR`, so a
//! developer's `~/.coderex/settings.json` cannot override the cassette URL or
//! model.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// The model used for recording and replay. All model switching in the tests
/// is canonicalized away by the cassette, so one model is enough.
pub const MODEL: &str = "deepseek-flash";

pub struct Cassette {
    base_url: String,
    config_home: tempfile::TempDir,
    child: Mutex<Child>,
}

impl Drop for Cassette {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Cassette {
    /// The `http://host:port` base URL the server bound.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Point a child `coderex` command at the cassette server.
    pub fn configure(&self, cmd: &mut Command) {
        cmd.env("ANTHROPIC_BASE_URL", &self.base_url);
        cmd.env("ANTHROPIC_API_KEY", "cassette");
        cmd.env("ANTHROPIC_AUTH_TOKEN", "cassette");
        cmd.env("ANTHROPIC_MODEL", MODEL);
        cmd.env("CLAUDE_CONFIG_DIR", self.config_home.path());
        for key in [
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
            "API_TIMEOUT_MS",
        ] {
            cmd.env_remove(key);
        }
    }

    /// Configure a child that runs in `cwd`.
    ///
    /// The working directory travels as a request header so replay can rewrite
    /// absolute paths in recorded answers (a recorded `cd /tmp/.tmpXXXX &&
    /// cargo test` must point at this run's fixture, not the recording's).
    pub fn configure_for(&self, cmd: &mut Command, cwd: &Path) {
        self.configure(cmd);
        cmd.env(
            "CODEREX_CUSTOM_HEADERS",
            format!("X-Cassette-Cwd: {}", cwd.display()),
        );
    }
}

static SERVERS: OnceLock<Mutex<HashMap<String, Arc<Cassette>>>> = OnceLock::new();

/// The cassette server for a suite, started on first use.
pub fn cassette(suite: &str) -> Arc<Cassette> {
    let servers = SERVERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = servers.lock().expect("cassette registry");
    if let Some(existing) = guard.get(suite) {
        return Arc::clone(existing);
    }
    let server = Arc::new(start(suite));
    guard.insert(suite.to_string(), Arc::clone(&server));
    server
}

fn cassette_dir(suite: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("cassettes")
        .join(suite)
}

fn has_cassettes(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
    })
}

fn start(suite: &str) -> Cassette {
    let dir = cassette_dir(suite);
    let record = std::env::var("CODEREX_CASSETTE_MODE").as_deref() == Ok("record");

    let mut command = Command::new(env!("CARGO_BIN_EXE_llm-cassette"));
    command
        .arg("--dir")
        .arg(&dir)
        .arg("--port")
        .arg("0")
        .arg("--model")
        .arg(MODEL)
        .arg("--exit-on-stdin-eof")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if record {
        let upstream = std::env::var("CODEREX_CASSETTE_UPSTREAM").unwrap_or_else(|_| {
            panic!(
                "record mode needs CODEREX_CASSETTE_UPSTREAM (the real Anthropic-compatible endpoint)"
            )
        });
        let api_key = std::env::var("CODEREX_CASSETTE_API_KEY")
            .unwrap_or_else(|_| panic!("record mode needs CODEREX_CASSETTE_API_KEY"));
        command
            .arg("--mode")
            .arg("record")
            .arg("--upstream")
            .arg(upstream)
            .arg("--api-key")
            .arg(api_key);
    } else {
        assert!(
            has_cassettes(&dir),
            "no cassettes in {}; record them once with CODEREX_CASSETTE_MODE=record",
            dir.display()
        );
        command.arg("--mode").arg("replay");
    }

    let mut child = command.spawn().expect("spawn llm-cassette");
    let stderr = child.stderr.take().expect("cassette stderr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut announced = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !announced && let Some(address) = line.split("listening on ").nth(1) {
                let _ = tx.send(address.trim().to_string());
                announced = true;
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("llm-cassette did not announce its address");

    let config_home = tempfile::tempdir().expect("cassette config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed cassette config home");

    Cassette {
        base_url: format!("http://{address}"),
        config_home,
        child: Mutex::new(child),
    }
}

/// Every provider-backed suite replays a cassette, so the environment is
/// always "available".
pub const fn provider_available() -> bool {
    true
}
