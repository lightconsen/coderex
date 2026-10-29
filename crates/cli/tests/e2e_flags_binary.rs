//! `--from-pr` and `--tmux` through the real binary.
//!
//! Both flags shell out to an external program (`gh`, `tmux`), so the test
//! supplies a fake one on `PATH` and asserts what the binary did with it.
//! The fakes are `sh` scripts, so the file is Unix-only.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

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

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake program");
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn path_with(dir: &Path) -> String {
    let base = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    format!("{}:{base}", dir.display())
}

/// Run one headless turn with `args`, returning stdout, stderr and status.
fn run(
    cwd: &Path,
    base_url: &str,
    args: &[&str],
    path: &str,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let mut command = Command::new(env!("CARGO_BIN_EXE_coderex"));
    command
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODEREX_LOG", "off")
        .env("PATH", path)
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

#[test]
fn from_pr_fetches_the_pull_request_through_gh() {
    let fake = tempfile::tempdir().expect("fake bin");
    let marker = fake.path().join("gh-calls");
    script(
        fake.path(),
        "gh",
        &format!(
            "printf '%s\\n' \"$*\" >> {}\n\
             case \"$1 $2\" in\n\
               'pr view') printf '%s' '{{\"title\":\"Fix the parser\",\"body\":\"Body text\",\"files\":[],\"commits\":[]}}' ;;\n\
               'pr diff') printf '%s' 'diff --git a/src/lib.rs b/src/lib.rs' ;;\n\
             esac\n",
            marker.display()
        ),
    );
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();
    let output = run(
        dir.path(),
        &base_url,
        &[
            "-p",
            "--dangerously-skip-permissions",
            "--from-pr",
            "https://github.com/lightconsen/coderex/pull/42",
            "hello",
        ],
        &path_with(fake.path()),
        &[],
    );
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "--from-pr failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = std::fs::read_to_string(&marker).expect("gh was never invoked");
    assert!(calls.contains("pr view"), "{calls}");
    assert!(calls.contains("pr diff"), "{calls}");
    assert!(
        calls.contains("https://github.com/lightconsen/coderex/pull/42"),
        "{calls}"
    );
}

#[test]
fn from_pr_reports_a_failed_fetch_instead_of_carrying_on() {
    let fake = tempfile::tempdir().expect("fake bin");
    script(fake.path(), "gh", "echo 'not logged in' >&2\nexit 1");
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();
    let output = run(
        dir.path(),
        &base_url,
        &[
            "-p",
            "--dangerously-skip-permissions",
            "--from-pr",
            "https://github.com/lightconsen/coderex/pull/42",
            "hello",
        ],
        &path_with(fake.path()),
        &[],
    );
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        !output.status.success(),
        "a failed --from-pr still exited 0: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not logged in") || stderr.contains("gh"),
        "{stderr}"
    );
}

#[test]
fn tmux_retitles_the_current_pane_when_already_inside_tmux() {
    let fake = tempfile::tempdir().expect("fake bin");
    let marker = fake.path().join("tmux-calls");
    script(
        fake.path(),
        "tmux",
        &format!("printf '%s\\n' \"$*\" >> {}\nexit 0", marker.display()),
    );
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();
    let output = run(
        dir.path(),
        &base_url,
        &["-p", "--dangerously-skip-permissions", "--tmux", "hello"],
        &path_with(fake.path()),
        &[("CODEREX_TMUX_ACTIVE", "1")],
    );
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "--tmux inside a pane failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = std::fs::read_to_string(&marker).expect("tmux was never invoked");
    assert!(
        calls.contains("rename-window coderex:"),
        "the pane was not retitled: {calls}"
    );
    assert!(
        calls.contains("automatic-rename off"),
        "automatic-rename was not disabled: {calls}"
    );
}

#[test]
fn tmux_outside_a_terminal_explains_itself() {
    let fake = tempfile::tempdir().expect("fake bin");
    script(fake.path(), "tmux", "exit 0");
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();
    let output = run(
        dir.path(),
        &base_url,
        &["-p", "--dangerously-skip-permissions", "--tmux", "hello"],
        &path_with(fake.path()),
        &[],
    );
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        !output.status.success(),
        "--tmux with piped stdin should not start a session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("interactive terminal"),
        "the refusal does not say why: {stderr}"
    );
}
