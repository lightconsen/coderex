//! Every slash command that declares non-interactive support is reachable
//! through the real binary: the turn completes and the command is never
//! reported as unknown.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

fn run_command(cwd: &Path, home: &Path, base_url: &str, name: &str) -> (bool, String) {
    run_command_with(cwd, home, base_url, name, &[])
}

fn run_command_with(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    name: &str,
    extra_env: &[(&str, &str)],
) -> (bool, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coderex"));
    command
        .args(["-p", &format!("/{name}")])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEREX_LOG", "off")
        .env("CODEREX_DISABLE_ADOPT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn coderex");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait().expect("poll coderex") {
            Some(status) => {
                let output = child.wait_with_output().expect("output");
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return (status.success(), text);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return (false, format!("/{name} did not finish within 20s"));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
fn every_non_interactive_slash_command_runs() {
    let mut registry = coderex_commands::registry::CommandRegistry::new();
    coderex_commands::registry::register_all_commands(&mut registry);
    let names: Vec<String> = registry
        .list()
        .iter()
        .filter(|command| command.supports_non_interactive)
        .map(|command| command.name.to_string())
        .collect();
    // 76 commands, 72 of which declare non-interactive support. Pin both so a
    // command that loses the flag (or a new one that forgets it) fails here
    // instead of silently dropping out of the sweep.
    assert_eq!(
        names.len(),
        72,
        "the sweep no longer covers the registry: {names:?}"
    );
    for excluded in ["heapdump", "mock-limits", "voice", "stickers"] {
        assert!(
            !names.iter().any(|name| name == excluded),
            "/{excluded} is interactive-only but was swept"
        );
    }

    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let (mut provider, base_url) = start_static_provider();

    let mut failures = Vec::new();
    for name in &names {
        let (success, output) = run_command(workspace.path(), home.path(), &base_url, name);
        if !success || output.contains("Unknown command") {
            failures.push(format!("/{name}: {output}"));
        }
    }

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        failures.is_empty(),
        "{} command(s) were not usable in the binary:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

/// The four commands that declare no non-interactive support still run through
/// the binary. They are excluded from the sweep because they exist for an
/// interactive session, not because they are broken, and nothing else covered
/// them.
#[test]
fn the_interactive_only_commands_still_answer_in_a_headless_run() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let (mut provider, base_url) = start_static_provider();
    let flags = [(
        "CODEREX_FEATURE_FLAGS",
        "heapdump,mock_limits,stickers,voice_mode",
    )];

    // Each command has to say something that is neither a crash nor an
    // "unknown command".
    for (name, expected) in [
        ("heapdump", "snapshot"),
        ("mock-limits", "limit"),
        ("stickers", "not implemented"),
        ("voice", "microphone"),
    ] {
        let (success, output) =
            run_command_with(workspace.path(), home.path(), &base_url, name, &flags);
        assert!(success, "/{name} failed: {output}");
        assert!(
            !output.contains("Unknown command"),
            "/{name} is not registered: {output}"
        );
        assert!(
            output.to_lowercase().contains(expected),
            "/{name} did not answer as expected ({expected:?}): {output}"
        );
    }

    let _ = provider.kill();
    let _ = provider.wait();
}
