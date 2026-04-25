//! Session teleportation — share and restore conversation state across directories.
//!
//! Provides `TeleportManager` for:
//! - Saving session snapshots to a target directory
//! - Loading session snapshots when changing working directory
//! - Generating share tokens for cross-machine session sharing
//! - Joining shared sessions by token
//! - Delta sync for merging concurrent session changes

use crate::delta_sync::{StateDelta, compute_delta, compute_revision_hash};
use coderex_core::state::SessionState;
use coderex_core::types::{Message, UsageSummary};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A portable session snapshot for teleportation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeleportSnapshot {
    /// Session identifier.
    pub session_id: String,
    /// Optional session title.
    pub title: Option<String>,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Token usage summary.
    pub usage: UsageSummary,
    /// Cumulative cost in USD.
    pub cost_usd: f64,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// Working directory at save time.
    pub cwd: String,
    /// Revision hash of this snapshot (for delta sync).
    pub revision: String,
}

impl TeleportSnapshot {
    /// Create a snapshot from a `SessionState`.
    #[must_use]
    pub fn from_session(state: &SessionState) -> Self {
        let state_map = Self::to_state_map(state);
        Self {
            session_id: state.session_id.to_string(),
            title: state.title.clone(),
            messages: state.messages.clone(),
            usage: state.usage,
            cost_usd: state.cost_usd,
            created_at: state.created_at.to_rfc3339(),
            cwd: state.cwd.to_string_lossy().to_string(),
            revision: compute_revision_hash(&state_map),
        }
    }

    /// Convert this snapshot back into a `SessionState`.
    ///
    /// The returned state will have a new `session_id` and `created_at`
    /// if deserialization fails for those fields.
    #[must_use]
    pub fn to_session_state(&self) -> SessionState {
        let mut state = SessionState::new(PathBuf::from(&self.cwd));
        if let Ok(id) = self.session_id.parse() {
            state.session_id = id;
        }
        state.title = self.title.clone();
        state.messages.clone_from(&self.messages);
        state.usage = self.usage;
        state.cost_usd = self.cost_usd;
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&self.created_at) {
            state.created_at = dt.with_timezone(&chrono::Utc);
        }
        state
    }

    /// Convert a `SessionState` into a flat key-value map for delta computation.
    fn to_state_map(state: &SessionState) -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("session_id".into(), state.session_id.to_string());
        map.insert("title".into(), state.title.clone().unwrap_or_default());
        map.insert("cost_usd".into(), state.cost_usd.to_string());
        map.insert("created_at".into(), state.created_at.to_rfc3339());
        map.insert("cwd".into(), state.cwd.to_string_lossy().to_string());
        map.insert("usage_input".into(), state.usage.input_tokens.to_string());
        map.insert("usage_output".into(), state.usage.output_tokens.to_string());
        map.insert(
            "usage_cache_read".into(),
            state.usage.cache_read_tokens.to_string(),
        );
        map.insert(
            "usage_cache_creation".into(),
            state.usage.cache_creation_tokens.to_string(),
        );
        for (i, msg) in state.messages.iter().enumerate() {
            if let Ok(json) = serde_json::to_string(msg) {
                map.insert(format!("msg_{i}"), json);
            }
        }
        map
    }

    /// Compute a `StateDelta` from `old` to `self`.
    #[must_use]
    pub fn delta_from(&self, old: &TeleportSnapshot) -> StateDelta {
        let old_state = Self::to_state_map(&old.to_session_state());
        let new_state = Self::to_state_map(&self.to_session_state());
        compute_delta(&old.revision, &old_state, &new_state)
    }
}

/// Errors from teleport operations.
#[derive(Debug, thiserror::Error)]
pub enum TeleportError {
    /// I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Serialization failure.
    #[error("serialization error: {0}")]
    Serialize(String),
    /// Deserialization failure.
    #[error("deserialization error: {0}")]
    Deserialize(String),
    /// Shared session not found.
    #[error("shared session not found: {0}")]
    SessionNotFound(String),
    /// Invalid token.
    #[error("invalid share token: {0}")]
    InvalidToken(String),
}

/// Manager for session teleportation and sharing.
#[derive(Debug, Clone)]
pub struct TeleportManager {
    /// Base directory for shared session storage.
    share_base: PathBuf,
}

impl TeleportManager {
    /// Create a new manager with the default share base (`~/.coderex/teleport/shared`).
    #[must_use]
    pub fn new() -> Self {
        let share_base = home::home_dir()
            .unwrap_or_default()
            .join(".coderex")
            .join("teleport")
            .join("shared");
        Self { share_base }
    }

    /// Create a manager with a custom share base.
    #[must_use]
    pub fn with_share_base(share_base: PathBuf) -> Self {
        Self { share_base }
    }

    /// Path to the teleport state file inside a directory.
    #[must_use]
    pub fn state_file_for(dir: &Path) -> PathBuf {
        dir.join(".coderex").join("teleport_state.json")
    }

    /// Save a session snapshot to the given directory's `.coderex/teleport_state.json`.
    pub fn save_to_directory(
        &self,
        state: &SessionState,
        dir: &Path,
    ) -> Result<PathBuf, TeleportError> {
        let coderex_dir = dir.join(".coderex");
        std::fs::create_dir_all(&coderex_dir)?;
        let path = coderex_dir.join("teleport_state.json");
        let snapshot = TeleportSnapshot::from_session(state);
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| TeleportError::Serialize(e.to_string()))?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// Load a session snapshot from a directory's `.coderex/teleport_state.json`.
    ///
    /// Returns `None` if the file does not exist.
    pub fn load_from_directory(&self, dir: &Path) -> Result<Option<SessionState>, TeleportError> {
        let path = Self::state_file_for(dir);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let snapshot: TeleportSnapshot = serde_json::from_str(&content)
            .map_err(|e| TeleportError::Deserialize(e.to_string()))?;
        Ok(Some(snapshot.to_session_state()))
    }

    /// Generate a share token and save the session snapshot.
    ///
    /// Returns the token string (8-character alphanumeric).
    pub fn share_session(&self, state: &SessionState) -> Result<String, TeleportError> {
        let token = Self::generate_token();
        std::fs::create_dir_all(&self.share_base)?;
        let path = self.share_base.join(format!("{token}.json"));
        let snapshot = TeleportSnapshot::from_session(state);
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| TeleportError::Serialize(e.to_string()))?;
        std::fs::write(&path, json)?;
        Ok(token)
    }

    /// Join a shared session by token.
    ///
    /// Returns the deserialized `SessionState`.
    pub fn join_session(&self, token: &str) -> Result<SessionState, TeleportError> {
        if token.len() != 8 || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(TeleportError::InvalidToken(token.into()));
        }
        let path = self.share_base.join(format!("{token}.json"));
        if !path.exists() {
            return Err(TeleportError::SessionNotFound(token.into()));
        }
        let content = std::fs::read_to_string(&path)?;
        let snapshot: TeleportSnapshot = serde_json::from_str(&content)
            .map_err(|e| TeleportError::Deserialize(e.to_string()))?;
        Ok(snapshot.to_session_state())
    }

    /// List active share tokens.
    #[must_use]
    pub fn list_shares(&self) -> Vec<String> {
        if !self.share_base.exists() {
            return Vec::new();
        }
        std::fs::read_dir(&self.share_base)
            .ok()
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let path = e.path();
                        if path.extension().is_some_and(|ext| ext == "json") {
                            path.file_stem().map(|s| s.to_string_lossy().to_string())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Revoke a share token, deleting its stored snapshot.
    pub fn revoke_share(&self, token: &str) -> Result<bool, TeleportError> {
        let path = self.share_base.join(format!("{token}.json"));
        if path.exists() {
            std::fs::remove_file(&path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Compute the delta between two session states.
    #[must_use]
    pub fn compute_delta(old: &SessionState, new: &SessionState) -> StateDelta {
        let old_snapshot = TeleportSnapshot::from_session(old);
        let new_snapshot = TeleportSnapshot::from_session(new);
        new_snapshot.delta_from(&old_snapshot)
    }

    fn generate_token() -> String {
        use rand::Rng;
        const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::thread_rng();
        (0..8)
            .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
            .collect()
    }
}

impl Default for TeleportManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::Message;
    use std::path::PathBuf;

    #[test]
    fn snapshot_roundtrip() {
        let mut state = SessionState::new(PathBuf::from("/tmp/test"));
        state.messages.push(Message::user("hello"));
        state.messages.push(Message::assistant("world"));
        state.title = Some("Test Session".into());
        state.cost_usd = 0.01;

        let snapshot = TeleportSnapshot::from_session(&state);
        let restored = snapshot.to_session_state();

        assert_eq!(restored.session_id, state.session_id);
        assert_eq!(restored.title, state.title);
        assert_eq!(restored.messages.len(), state.messages.len());
        assert_eq!(restored.cost_usd, state.cost_usd);
        assert_eq!(restored.cwd, state.cwd);
    }

    #[test]
    fn save_and_load_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TeleportManager::new();

        let mut state = SessionState::new(PathBuf::from("/tmp/test"));
        state.messages.push(Message::user("hello"));

        let path = mgr.save_to_directory(&state, tmp.path()).unwrap();
        assert!(path.exists());

        let loaded = mgr.load_from_directory(tmp.path()).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn load_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TeleportManager::new();
        let loaded = mgr.load_from_directory(tmp.path()).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn share_and_join() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TeleportManager::with_share_base(tmp.path().to_path_buf());

        let mut state = SessionState::new(PathBuf::from("/tmp/test"));
        state.messages.push(Message::user("shared msg"));

        let token = mgr.share_session(&state).unwrap();
        assert_eq!(token.len(), 8);

        let joined = mgr.join_session(&token).unwrap();
        assert_eq!(joined.messages.len(), 1);
    }

    #[test]
    fn join_invalid_token() {
        let mgr = TeleportManager::new();
        let result = mgr.join_session("bad!");
        assert!(matches!(result, Err(TeleportError::InvalidToken(_))));
    }

    #[test]
    fn join_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TeleportManager::with_share_base(tmp.path().to_path_buf());
        let result = mgr.join_session("abc12345");
        assert!(matches!(result, Err(TeleportError::SessionNotFound(_))));
    }

    #[test]
    fn list_shares() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TeleportManager::with_share_base(tmp.path().to_path_buf());

        let state = SessionState::new(PathBuf::from("/tmp/test"));
        let token = mgr.share_session(&state).unwrap();

        let shares = mgr.list_shares();
        assert!(shares.contains(&token));
    }

    #[test]
    fn revoke_share() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TeleportManager::with_share_base(tmp.path().to_path_buf());

        let state = SessionState::new(PathBuf::from("/tmp/test"));
        let token = mgr.share_session(&state).unwrap();

        assert!(mgr.revoke_share(&token).unwrap());
        assert!(!mgr.revoke_share(&token).unwrap());
    }

    #[test]
    fn compute_session_delta() {
        let mut old = SessionState::new(PathBuf::from("/tmp/test"));
        old.messages.push(Message::user("old"));

        let mut new = SessionState::new(PathBuf::from("/tmp/test"));
        new.messages.push(Message::user("old"));
        new.messages.push(Message::assistant("new reply"));

        let delta = TeleportManager::compute_delta(&old, &new);
        assert!(!delta.is_empty());
    }
}
