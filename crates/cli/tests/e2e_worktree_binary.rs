//! `--worktree` through the real binary: the session runs in its own checkout,
//! so the model's edits never land in the original working tree.

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

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"worktree_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["add", "Cargo.toml", "src/lib.rs"][..],
        &["commit", "--quiet", "-m", "initial"][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }
}

#[test]
fn worktree_mode_edits_the_worktree_not_the_original_checkout() {
    let repo = tempfile::tempdir().expect("repo");
    init_repo(repo.path());
    let original = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    let (mut provider, base_url) = start_static_provider();
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--worktree", "--dangerously-skip-permissions"])
        .current_dir(repo.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
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

    assert_eq!(
        std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        original,
        "the original checkout was edited"
    );

    let worktrees = repo.path().join(".coderex").join("worktrees");
    let written: Vec<PathBuf> = std::fs::read_dir(&worktrees)
        .expect("a worktree directory")
        .flatten()
        .map(|entry| entry.path().join("src/lib.rs"))
        .filter(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains("fibonacci")))
        .collect();
    assert_eq!(
        written.len(),
        1,
        "expected exactly one worktree with the model's edit, found {written:?}"
    );
    assert!(
        written[0]
            .parent()
            .and_then(|src| src.parent())
            .is_some_and(|root| root.join(".git").exists()),
        "the edited path is not a git worktree: {}",
        written[0].display()
    );
}
