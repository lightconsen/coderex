//! A macOS managed-preferences plist reaches the real binary's resolved
//! settings: only an administrator-controlled carrier may enable a
//! security-sensitive permission mode.

use std::io::{BufRead, BufReader, Write};
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

/// The XML subset `plist` reads: a dictionary of a few scalar keys.
fn write_plist(path: &Path) {
    std::fs::write(
        path,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>config_version</key><integer>4</integer>
    <key>permission_mode</key><string>bypass-permissions</string>
    <key>telemetry_enabled</key><false/>
</dict>
</plist>
"#,
    )
    .expect("write plist");
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

fn run(cwd: &Path, home: &Path, base_url: &str, plist: Option<&Path>) -> std::process::Output {
    run_prompt(
        cwd,
        home,
        base_url,
        plist,
        "add a recursive fibonacci function to src/lib.rs with unit tests",
    )
}

fn run_prompt(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    plist: Option<&Path>,
    prompt: &str,
) -> std::process::Output {
    run_prompt_with_format(cwd, home, base_url, plist, prompt, false)
}

fn run_prompt_with_format(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    plist: Option<&Path>,
    prompt: &str,
    stream_json: bool,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coderex"));
    command.arg("-p");
    if stream_json {
        command.args(["--output-format", "stream-json"]);
    }
    command
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match plist {
        Some(path) => {
            command.env("CODEREX_MANAGED_SETTINGS_PLIST_PATH", path);
        }
        None => {
            command.env_remove("CODEREX_MANAGED_SETTINGS_PLIST_PATH");
        }
    }
    let mut child = command.spawn().expect("spawn coderex");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{prompt}\n").as_bytes())
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
fn a_managed_plist_can_enable_bypass_that_user_settings_cannot() {
    let home = config_home();

    let without = workspace();
    let (mut provider, base_url) = start_static_provider();
    let denied = run(without.path(), home.path(), &base_url, None);
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(denied.status.success(), "{denied:?}");
    assert!(
        !std::fs::read_to_string(without.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "a plain session bypassed permissions"
    );

    let managed_dir = tempfile::tempdir().expect("managed");
    let plist = managed_dir.path().join("dev.coderex.plist");
    write_plist(&plist);
    let with = workspace();
    let (mut provider, base_url) = start_static_provider();
    let allowed = run(with.path(), home.path(), &base_url, Some(&plist));
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        allowed.status.success(),
        "coderex failed with the managed plist: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(
        std::fs::read_to_string(with.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "the managed plist's permission mode was not applied"
    );
}

/// A managed plist can carry permission rules, and they reach the binary.
#[test]
fn a_managed_plist_can_allow_a_command() {
    let home = config_home();
    let managed_dir = tempfile::tempdir().expect("managed");
    let plist = managed_dir.path().join("dev.coderex.plist");
    std::fs::write(
        &plist,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>config_version</key><integer>4</integer>
    <key>telemetry_enabled</key><false/>
    <key>permissions</key>
    <dict>
        <key>allow</key>
        <array><string>Bash(echo:*)</string></array>
        <key>deny</key>
        <array><string>Read(./.env)</string></array>
    </dict>
</dict>
</plist>
"#,
    )
    .expect("write plist");

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    let (mut provider, base_url) = start_static_provider();
    let output = run_prompt_with_format(
        workspace.path(),
        home.path(),
        &base_url,
        Some(&plist),
        "run a safe echo",
        true,
    );
    let _ = provider.kill();
    let _ = provider.wait();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}\n{stdout}");
    assert!(
        stdout.contains("auto-safe"),
        "the managed allow rule did not reach the session:\n{stdout}\n{stderr}"
    );
}
