//! Plugin install → load → execute through the real binary: a local plugin is
//! installed, its process starts, and its slash command runs.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const PLUGIN_PROCESS: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

def respond(rid, result):
    print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method")
    rid = request.get("id")
    if rid is None:
        continue
    if method == "coderex/initialize":
        respond(rid, {"protocolVersion": "1.0"})
    elif method == "coderex/commands/list":
        respond(rid, [{"name": "hello", "description": "Say hello"}])
    elif method == "coderex/skills/list":
        respond(rid, [])
    elif method == "coderex/commands/execute":
        params = request.get("params") or {}
        args = params.get("args") or []
        greeting = os.environ.get("CODEREX_PLUGIN_OPTION_GREETING", "")
        respond(rid, "plugin-hello:" + " ".join(str(arg) for arg in args) + "|" + greeting)
    elif method == "coderex/ping":
        respond(rid, {})
    else:
        respond(rid, {})
"#;

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

fn write_plugin_source(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let source = dir.join("plugin-src");
    std::fs::create_dir_all(source.join("bin")).expect("plugin bin");
    std::fs::write(
        source.join("plugin.json"),
        serde_json::json!({
            "name": "mock-plugin",
            "version": "1.0.0",
            "description": "test plugin",
            "commands": ["hello"],
            "entrypoint": "bin/plugin",
            "defaultEnabled": true,
            "userConfig": {"GREETING": {"type": "string", "required": true}}
        })
        .to_string(),
    )
    .expect("manifest");
    let script = source.join("bin/plugin");
    std::fs::write(&script, PLUGIN_PROCESS).expect("plugin script");
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
    }
    source
}

fn run(
    cwd: &Path,
    home: &Path,
    args: &[&str],
    prompt: Option<&str>,
    base_url: &str,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("HOME", home)
        .env("CODEREX_LOG", "off")
        .stdin(if prompt.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex");
    if let Some(prompt) = prompt {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(format!("{prompt}\n").as_bytes())
            .expect("write prompt");
    }
    child.wait_with_output().expect("wait")
}

#[test]
fn an_installed_plugin_process_serves_its_slash_command() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        serde_json::json!({
            "config_version": 4,
            "telemetry_enabled": false,
            "plugins": {
                "mock-plugin": {"options": {"GREETING": "hi-from-settings"}}
            }
        })
        .to_string(),
    )
    .expect("settings");
    let source = write_plugin_source(workspace.path());
    let (mut provider, base_url) = start_static_provider();

    let install = run(
        workspace.path(),
        home.path(),
        &["plugin", "install", source.to_str().unwrap()],
        None,
        &base_url,
    );
    let install_stdout = String::from_utf8_lossy(&install.stdout);
    assert!(
        install_stdout.contains("Plugin installed to:"),
        "install failed: {install_stdout}\n{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let installed = workspace
        .path()
        .join(".coderex")
        .join("plugins")
        .join("mock-plugin");
    assert!(installed.join("bin/plugin").exists(), "{installed:?}");

    let list = run(
        workspace.path(),
        home.path(),
        &["plugin", "list"],
        None,
        &base_url,
    );
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("mock-plugin"),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );

    let output = run(
        workspace.path(),
        home.path(),
        &["-p"],
        Some("/mock-plugin/hello world"),
        &base_url,
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "the plugin command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("plugin-hello:world|hi-from-settings"),
        "the plugin process never answered: {stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_with_key(
    cwd: &Path,
    home: &Path,
    args: &[&str],
    key: &str,
    base_url: &str,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("HOME", home)
        .env("CODEREX_LOG", "off")
        .env("CODEREX_PLUGIN_PUBLIC_KEY", key)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run coderex")
}

/// A directory has no archive signature, so a configured signing key accepts
/// it only when trust-on-first-use already pinned those exact contents.
#[test]
fn a_signing_key_enforces_the_trust_on_first_use_pin() {
    const KEY: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let source = write_plugin_source(workspace.path());
    let (mut provider, base_url) = start_static_provider();
    let plugin_dir = workspace.path().join(".coderex/plugins/mock-plugin");
    let pins = workspace.path().join(".coderex/plugins/.coderex-pins.json");

    // No key: the install is unverified, and it records what it got.
    let first = run(
        workspace.path(),
        home.path(),
        &["plugin", "install", source.to_str().unwrap()],
        None,
        &base_url,
    );
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("Plugin installed to:"),
        "the unkeyed install failed: {}\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(pins.exists(), "no trust-on-first-use pin was recorded");
    std::fs::remove_dir_all(&plugin_dir).expect("remove the installed copy");

    // The same contents are accepted with a key configured.
    let matching = run_with_key(
        workspace.path(),
        home.path(),
        &["plugin", "install", source.to_str().unwrap()],
        KEY,
        &base_url,
    );
    assert!(
        String::from_utf8_lossy(&matching.stdout).contains("Plugin installed to:"),
        "the keyed install of the pinned contents failed: {}\n{}",
        String::from_utf8_lossy(&matching.stdout),
        String::from_utf8_lossy(&matching.stderr)
    );
    std::fs::remove_dir_all(&plugin_dir).expect("remove the installed copy");

    // Changed contents no longer match the pin: refuse rather than replace.
    let mut script = std::fs::read_to_string(source.join("bin/plugin")).unwrap();
    script.push_str("\n# changed after the pin\n");
    std::fs::write(source.join("bin/plugin"), script).unwrap();
    let changed = run_with_key(
        workspace.path(),
        home.path(),
        &["plugin", "install", source.to_str().unwrap()],
        KEY,
        &base_url,
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        !changed.status.success(),
        "changed contents were installed over the pin: {}",
        String::from_utf8_lossy(&changed.stdout)
    );
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&changed.stdout),
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(
        message.contains("does not match") || message.contains("pinned"),
        "the refusal does not explain the pin: {message}"
    );
    assert!(
        !plugin_dir.exists(),
        "a refused install left a plugin directory behind"
    );
}
