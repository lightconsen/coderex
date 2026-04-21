//! Crash recovery — session state snapshot/restore via `.coderex/recovery.json`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Recovery snapshot written periodically during REPL sessions.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RecoveryState {
    /// Session identifier.
    pub session_id: String,
    /// Number of messages in the session.
    pub message_count: usize,
    /// Last user input buffer content.
    pub last_input: String,
    /// When this snapshot was saved (Unix timestamp seconds).
    pub saved_at: u64,
}

/// Path to the recovery file in `.coderex/`.
#[must_use]
pub fn recovery_file_path(cwd: &str) -> PathBuf {
    let mut path = PathBuf::from(cwd);
    path.push(".coderex/recovery.json");
    path
}

/// Save a recovery snapshot.
pub fn save(cwd: &str, state: &RecoveryState) -> std::io::Result<()> {
    let path = recovery_file_path(cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, content)
}

/// Load the recovery snapshot, if it exists.
pub fn load(cwd: &str) -> Option<RecoveryState> {
    let path = recovery_file_path(cwd);
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Remove the recovery file.
pub fn remove(cwd: &str) -> std::io::Result<()> {
    let path = recovery_file_path(cwd);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Check if the snapshot is recent (within the given duration).
#[must_use]
pub fn is_recent(cwd: &str, max_age: Duration) -> bool {
    let Some(state) = load(cwd) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    now.saturating_sub(state.saved_at) < max_age.as_secs()
}

/// Create a new recovery state with the current timestamp.
#[must_use]
pub fn new_state(session_id: String, message_count: usize, last_input: String) -> RecoveryState {
    let saved_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    RecoveryState {
        session_id,
        message_count,
        last_input,
        saved_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let state = new_state("test_session".into(), 5, "hello world".into());
        save(&cwd, &state).unwrap();
        let loaded = load(&cwd).expect("should load recovery state");
        assert_eq!(loaded.session_id, "test_session");
        assert_eq!(loaded.message_count, 5);
        assert_eq!(loaded.last_input, "hello world");
    }

    #[test]
    fn load_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy();
        assert!(load(&cwd).is_none());
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy();
        remove(&cwd).unwrap();
        remove(&cwd).unwrap();
    }

    #[test]
    fn is_recent_with_fresh_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let state = new_state("s".into(), 0, String::new());
        save(&cwd, &state).unwrap();
        assert!(is_recent(&cwd, Duration::from_secs(60)));
    }

    #[test]
    fn is_recent_with_old_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy();
        // Manually write an old timestamp
        let state = RecoveryState {
            session_id: "s".into(),
            message_count: 0,
            last_input: String::new(),
            saved_at: 1_000_000_000, // year 2001
        };
        save(&cwd, &state).unwrap();
        assert!(!is_recent(&cwd, Duration::from_secs(60)));
    }

    #[test]
    fn is_recent_returns_false_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy();
        assert!(!is_recent(&cwd, Duration::from_secs(60)));
    }
}
