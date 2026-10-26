//! A mod's UI through the real binary's stream-json output: the element tree
//! crosses the worker boundary and is emitted as a `mod_ui` system frame.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
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
            if !announced
                && let Some(address) = line.split("listening on ").nth(1)
            {
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

#[test]
fn a_mod_ui_tree_reaches_the_stream_json_host() {
    let mods = tempfile::tempdir().expect("mods");
    std::fs::write(
        mods.path().join("hello.js"),
        "function register(on) { on('SessionStart', '*', function ($) { \
         $.ui.render($.ui.text('mod-says-hi')); }); }",
    )
    .expect("mod");
    let workspace = tempfile::tempdir().expect("workspace");
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let (mut provider, base_url) = start_static_provider();

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--dangerously-skip-permissions", "--output-format", "stream-json"])
        .current_dir(workspace.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
        .env("CODEREX_MODS", "1")
        .env("CODEREX_MODS_DIR", mods.path())
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
        .write_all(b"hello\n")
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let frame = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|frame| frame.to_string().contains("mod-says-hi"))
        .unwrap_or_else(|| panic!("no mod UI frame in:\n{stdout}"));
    assert_eq!(frame["type"], "system");
    assert_eq!(frame["subtype"], "mod_ui");
    assert!(
        frame.to_string().contains("\"text\":\"mod-says-hi\""),
        "{frame}"
    );
}
