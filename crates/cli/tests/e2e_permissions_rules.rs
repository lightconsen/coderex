//! Settings-defined permission rules through the real binary: project and user
//! layers, allow/deny/ask precedence, and a malformed layer falling back.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

#[allow(clippy::needless_pass_by_value)]
fn write_json(path: &Path, value: serde_json::Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create settings dir");
    }
    std::fs::write(path, value.to_string()).expect("write settings");
}

fn config_home(permissions: Option<serde_json::Value>) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    let mut settings = serde_json::json!({
        "config_version": 4,
        "telemetry_enabled": false,
    });
    if let Some(permissions) = permissions {
        settings["permissions"] = permissions;
    }
    write_json(&home.path().join("settings.json"), settings);
    home
}

/// Run one headless turn in Default mode. Stdin is closed after the prompt, so
/// an unanswered ask resolves as "no confirmation channel" instead of hanging.
fn run(cwd: &Path, home: &Path, base_url: &str, prompt: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--output-format", "stream-json"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{prompt}\n").as_bytes())
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "coderex failed: {stderr}\nstdout:\n{stdout}"
    );
    stdout.into_owned()
}

/// Run a turn whose tool call is expected to ask, and answer "deny" on the
/// control channel. Returns the collected stdout and whether a control request
/// was seen.
fn run_denying(cwd: &Path, home: &Path, base_url: &str, prompt: &str) -> (String, bool) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--output-format", "stream-json"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex");
    let mut stdin = Some(child.stdin.take().expect("stdin"));
    stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("{prompt}\n").as_bytes())
        .expect("write prompt");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut collected = String::new();
    let mut saw_control = false;
    let mut finished = false;
    while Instant::now() < deadline {
        let Ok(line) = rx.recv_timeout(Duration::from_secs(5)) else {
            break;
        };
        collected.push_str(&line);
        collected.push('\n');
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match frame["type"].as_str().unwrap_or_default() {
            "control_request" => {
                saw_control = true;
                let request_id = frame["request_id"].as_str().unwrap_or_default();
                let response = serde_json::json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": {"behavior": "deny", "message": "test"}
                    }
                });
                let pipe = stdin.as_mut().expect("stdin");
                pipe.write_all(format!("{response}\n").as_bytes())
                    .expect("write control response");
                pipe.flush().expect("flush control response");
                // Closing stdin is the host's "done answering" signal.
                drop(stdin.take());
            }
            "result" => {
                finished = true;
                break;
            }
            _ => {}
        }
    }
    drop(stdin.take());
    let status = child.wait().expect("wait");
    let stderr = child
        .stderr
        .take()
        .map(|mut pipe| {
            use std::io::Read as _;
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
        .unwrap_or_default();
    assert!(
        finished,
        "no result frame; stdout:\n{collected}\nstderr:\n{stderr}"
    );
    assert!(status.success(), "coderex failed: {stderr}");
    (collected, saw_control)
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    dir
}

#[test]
fn a_project_allow_rule_lets_a_command_run_without_a_prompt() {
    let dir = workspace();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({"permissions": {"allow": ["Bash(echo:*)"]}}),
    );
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();

    let stdout = run(dir.path(), home.path(), &base_url, "run a safe echo");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        stdout.contains("auto-safe"),
        "the allowed command did not run:\n{stdout}"
    );
    assert!(
        !stdout.contains("requires approval"),
        "the allowed command still asked:\n{stdout}"
    );
}

#[test]
fn without_the_rule_the_same_command_asks() {
    let dir = workspace();
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();

    let (stdout, asked) = run_denying(dir.path(), home.path(), &base_url, "run a safe echo");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(asked, "the control turn did not ask:\n{stdout}");
    assert!(stdout.contains("denied by user"), "{stdout}");
}

#[test]
fn a_deny_rule_blocks_a_command_before_it_runs() {
    let dir = workspace();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({"permissions": {"deny": ["Bash(rm:*)"]}}),
    );
    let sentinel = std::env::temp_dir().join(format!(
        "coderex-rule-sentinel-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&sentinel, "keep me").unwrap();
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();

    let stdout = run(
        dir.path(),
        home.path(),
        &base_url,
        "benchmark malicious shell",
    );

    let _ = provider.kill();
    let _ = provider.wait();
    let still_there = sentinel.exists();
    let _ = std::fs::remove_file(&sentinel);
    assert!(still_there, "the denied command removed its target");
    assert!(
        stdout.contains("denied") && stdout.contains("Bash(rm:*)"),
        "the rule denial did not reach the transcript:\n{stdout}"
    );
}

#[test]
fn an_ask_rule_beats_an_allow_rule() {
    let dir = workspace();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({
            "permissions": {
                "allow": ["Bash(echo:*)"],
                "ask": ["Bash(echo auto-safe:*)"]
            }
        }),
    );
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();

    let (stdout, asked) = run_denying(dir.path(), home.path(), &base_url, "run a safe echo");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        asked,
        "the ask rule did not produce a control request:\n{stdout}"
    );
    assert!(
        stdout.contains("Bash(echo auto-safe:*)") && stdout.contains("denied by user"),
        "the ask rule did not outrank the allow:\n{stdout}"
    );
}

#[test]
fn a_malformed_project_rule_is_skipped_and_user_rules_still_apply() {
    let dir = workspace();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({"permissions": {"deny": ["Bash("]}}),
    );
    let home = config_home(Some(serde_json::json!({"allow": ["Bash(echo:*)"]})));
    let (mut provider, base_url) = start_static_provider();

    let stdout = run(dir.path(), home.path(), &base_url, "run a safe echo");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        stdout.contains("auto-safe"),
        "the user allow rule did not survive the malformed project file:\n{stdout}"
    );
}

#[test]
fn a_path_deny_rule_blocks_a_read() {
    let dir = workspace();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({"permissions": {"deny": ["Read(./src/lib.rs)"]}}),
    );
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();

    let stdout = run(
        dir.path(),
        home.path(),
        &base_url,
        "benchmark secret leakage",
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        stdout.contains("denied") && stdout.contains("Read(./src/lib.rs)"),
        "the path deny did not reach the transcript:\n{stdout}"
    );
}

#[test]
fn a_compound_command_needs_an_allow_rule_for_every_segment() {
    let dir = workspace();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({"permissions": {"allow": ["Bash(cd:*)"]}}),
    );
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();
    let (stdout, asked) = run_denying(dir.path(), home.path(), &base_url, "run a chained command");
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(asked, "the uncovered segment did not ask:\n{stdout}");
    assert!(stdout.contains("denied by user"), "{stdout}");

    // Covering both segments lets the compound command run.
    let covered = workspace();
    write_json(
        &covered.path().join(".coderex/settings.json"),
        serde_json::json!({
            "permissions": {"allow": ["Bash(cd:*)", "Bash(echo:*)"]}
        }),
    );
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();
    let stdout = run(
        covered.path(),
        home.path(),
        &base_url,
        "run a chained command",
    );
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        stdout.contains("chained-ok"),
        "a fully covered compound command did not run:\n{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_path_cannot_bypass_a_path_deny() {
    let dir = workspace();
    let protected = dir.path().join("protected");
    std::fs::create_dir(&protected).unwrap();
    std::fs::write(protected.join("secret.txt"), "hidden-secret").unwrap();
    std::os::unix::fs::symlink(&protected, dir.path().join("link")).unwrap();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({"permissions": {"deny": ["Read(./protected/**)"]}}),
    );
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();

    let stdout = run(
        dir.path(),
        home.path(),
        &base_url,
        "read the symlinked file",
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        stdout.contains("denied") && stdout.contains("Read(./protected/**)"),
        "a symlinked path bypassed the deny:\n{stdout}"
    );
    assert!(
        !stdout.contains("hidden-secret"),
        "the denied file content reached the transcript:\n{stdout}"
    );
}

#[test]
fn an_opaque_wrapped_command_is_not_auto_approved_by_an_allow_rule() {
    let dir = workspace();
    write_json(
        &dir.path().join(".coderex/settings.json"),
        serde_json::json!({"permissions": {"allow": ["Bash(sudo:*)"]}}),
    );
    let home = config_home(None);
    let (mut provider, base_url) = start_static_provider();

    let (stdout, asked) = run_denying(dir.path(), home.path(), &base_url, "run a wrapped command");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(asked, "an opaque command was auto-approved:\n{stdout}");
    assert!(stdout.contains("denied"), "{stdout}");
}
