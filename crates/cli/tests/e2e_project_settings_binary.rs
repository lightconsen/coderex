//! Project settings cannot enable a security-sensitive permission mode: a
//! repository-controlled `.coderex/settings.json` that asks for bypass must
//! still be gated.

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
fn a_project_cannot_turn_on_bypass_permissions() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join(".coderex")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".coderex/settings.json"),
        r#"{"config_version": 4, "permission_mode": "bypass-permissions"}"#,
    )
    .unwrap();
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .unwrap();
    let (mut provider, base_url) = start_static_provider();

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p"])
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("HOME", home.path())
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
        .write_all(b"add a recursive fibonacci function to src/lib.rs with unit tests\n")
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "project settings enabled bypass and the write landed:\n{content}"
    );
}
