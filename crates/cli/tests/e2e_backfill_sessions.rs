//! `/backfill-sessions` writes the session index through the real binary.
//!
//! The command used to count transcript lines and say "Backfilling session
//! index...", leaving the index empty. A workspace whose transcripts predate
//! the index (or whose `sessions.db` was deleted) then had no searchable
//! sessions, which is exactly the case the command exists for.

use std::io::{BufRead, BufReader};
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

/// Write two transcripts through the store, the way an earlier session would
/// have, with no index beside them.
fn seed_transcripts(dir: &Path) {
    let store = coderex_memory::transcript::TranscriptStore::new(dir.join(".coderex/transcripts"));
    store
        .rewrite(
            "alpha",
            &[
                coderex_core::types::Message::user("investigate the oauth refresh bug"),
                coderex_core::types::Message::assistant("found it in the token rotation"),
            ],
        )
        .expect("alpha transcript");
    store
        .rewrite(
            "beta",
            &[coderex_core::types::Message::user(
                "revert the schema change",
            )],
        )
        .expect("beta transcript");
}

fn run_backfill(dir: &Path, base_url: &str) -> std::process::Output {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "/backfill-sessions"])
        .current_dir(dir)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODEREX_LOG", "off")
        .env("CODEREX_DISABLE_ADOPT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex");
    child.wait_with_output().expect("wait")
}

#[test]
fn backfill_writes_a_searchable_index_for_existing_transcripts() {
    let dir = tempfile::tempdir().expect("workspace");
    seed_transcripts(dir.path());
    assert!(
        !dir.path().join(".coderex/sessions.db").exists(),
        "the fixture already has an index"
    );
    let (mut provider, base_url) = start_static_provider();

    let output = run_backfill(dir.path(), &base_url);

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("Indexed 2 of 2 transcript file(s)"),
        "the command did not report what it wrote:\n{stdout}"
    );

    let db = dir.path().join(".coderex/sessions.db");
    assert!(db.exists(), "no index was written at {}", db.display());

    let index = coderex_memory::session_index::SessionIndex::open(&db).expect("open the index");
    // `project_path` is the session's working directory as the CLI resolved
    // it (macOS `/var` is really `/private/var`), so list everything rather
    // than re-deriving the string.
    let mut rows = index.list_recent(10).expect("list rows");
    rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    assert_eq!(
        rows.iter()
            .map(|row| row.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"],
        "the transcripts were not indexed: {rows:?}"
    );
    // A title is taken from the first user prompt, so `/resume` search has
    // something to show for a session that was never saved through the CLI.
    assert_eq!(
        rows[0].ai_title.as_deref(),
        Some("investigate the oauth refresh bug")
    );

    // The content, not just the metadata, is in the full-text table.
    let hits = index.search("rotation*", 10).expect("search");
    assert_eq!(hits.len(), 1, "the message content was not indexed");
    assert_eq!(hits[0].session_id, "alpha");
}

#[test]
fn backfill_on_an_empty_workspace_says_zero_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();

    let output = run_backfill(dir.path(), &base_url);

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("Indexed 0 of 0 transcript file(s)"),
        "{stdout}"
    );
}
