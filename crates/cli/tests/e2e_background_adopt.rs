//! A starting session settles background-job records a previous process left
//! `running`: it reports the interrupted work and marks the record honestly.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn start_static_provider() -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args([
            "--port",
            "0",
            "--responses",
            responses.to_str().expect("responses path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn static provider");

    let stderr = child.stderr.take().expect("server stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
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
        .expect("static provider did not announce its address");
    (child, format!("http://{address}"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        .try_into()
        .expect("fits")
}

/// A record written by an older CodeRex: no session id, still `running`.
fn seed_stale_record(cwd: &Path) {
    let jobs = cwd.join(".coderex").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    let now = now_ms();
    std::fs::write(
        jobs.join("legacy1.json"),
        serde_json::json!({
            "id": "legacy1",
            "kind": "agent",
            "description": "inspect the parser",
            "cwd": cwd.display().to_string(),
            "started_at_ms": now,
            "updated_at_ms": now,
            "state": "running"
        })
        .to_string(),
    )
    .unwrap();
}

fn run(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coderex"));
    command
        .args(["-p", "--dangerously-skip-permissions"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn coderex");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

fn config_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    home
}

#[test]
fn a_stale_running_record_is_reported_and_marked_interrupted() {
    let dir = tempfile::tempdir().expect("workspace");
    seed_stale_record(dir.path());
    let home = config_home();
    let (mut provider, base_url) = start_static_provider();

    let output = run(dir.path(), home.path(), &base_url, &[]);

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("legacy1") && stderr.contains("could not be resumed"),
        "the interrupted job was not reported:\n{stderr}"
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".coderex/jobs/legacy1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        record["state"], "interrupted",
        "the record still claims to run: {record}"
    );
}

#[test]
fn the_kill_switch_leaves_records_alone() {
    let dir = tempfile::tempdir().expect("workspace");
    seed_stale_record(dir.path());
    let home = config_home();
    let (mut provider, base_url) = start_static_provider();

    let output = run(
        dir.path(),
        home.path(),
        &base_url,
        &[("CODEREX_DISABLE_ADOPT", "1")],
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(output.status.success(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("could not be resumed"),
        "adoption ran despite the kill switch"
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".coderex/jobs/legacy1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["state"], "running", "{record}");
}
