//! Help and version are not first-run prompts: they must not ask for
//! telemetry consent or persist a decision.

use std::process::{Command, Stdio};

fn run(home: &std::path::Path, flag: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_coderex"))
        .arg(flag)
        .env("CLAUDE_CONFIG_DIR", home)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run coderex")
}

#[test]
fn help_and_version_do_not_trigger_telemetry_consent() {
    for flag in ["--version", "--help"] {
        let home = tempfile::tempdir().expect("config home");
        let output = run(home.path(), flag);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{flag} failed: {stderr}\n{stdout}");
        assert!(
            !stdout.contains("Enable telemetry?"),
            "{flag} asked for consent:\n{stdout}"
        );
        assert!(
            !stderr.contains("Enable telemetry?"),
            "{flag} asked for consent:\n{stderr}"
        );
        let settings = home.path().join("settings.json");
        if settings.exists() {
            let value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&settings).unwrap())
                    .expect("settings json");
            assert!(
                value.get("telemetry_enabled").is_none(),
                "{flag} persisted a telemetry decision: {value}"
            );
        }
    }
}

#[test]
fn version_names_the_binary() {
    let home = tempfile::tempdir().expect("config home");
    let output = run(home.path(), "--version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.to_lowercase().contains("coderex"),
        "unexpected version output: {stdout}"
    );
}
