//! Session persistence — save and restore permission / elicitation state.

use crate::{ElicitationTracker, PermissionTracker};
use std::path::{Path, PathBuf};

/// Error from session store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// I/O error.
    #[error("store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Serialization error.
    #[error("store serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// On-disk JSON store for a single session's tracker state.
#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
}

/// Serializable snapshot of session trackers.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SessionSnapshot {
    permissions: PermissionTracker,
    elicitations: ElicitationTracker,
}

impl SessionStore {
    /// Open the store for a given session ID.
    ///
    /// Files are written to `~/.coderex/sdk_sessions/{session_id}.json`.
    pub fn for_session(session_id: &str) -> Result<Self, StoreError> {
        let dir = coderex_config::config_home().join("sdk_sessions");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{session_id}.json"));
        Ok(Self { path })
    }

    /// Open the store at an explicit path (useful for tests).
    #[must_use]
    pub const fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Save tracker state to disk.
    pub async fn save(
        &self,
        permissions: &PermissionTracker,
        elicitations: &ElicitationTracker,
    ) -> Result<(), StoreError> {
        let snapshot = SessionSnapshot {
            permissions: permissions.clone(),
            elicitations: elicitations.clone(),
        };
        let json = serde_json::to_string_pretty(&snapshot)?;
        tokio::fs::write(&self.path, json).await?;
        Ok(())
    }

    /// Load tracker state from disk.
    ///
    /// Returns `(PermissionTracker, ElicitationTracker)`.
    pub async fn load(&self) -> Result<(PermissionTracker, ElicitationTracker), StoreError> {
        let json = tokio::fs::read_to_string(&self.path).await?;
        let snapshot: SessionSnapshot = serde_json::from_str(&json)?;
        Ok((snapshot.permissions, snapshot.elicitations))
    }

    /// Remove the persisted file, if it exists.
    pub async fn remove(&self) -> Result<(), StoreError> {
        if self.path.exists() {
            tokio::fs::remove_file(&self.path).await?;
        }
        Ok(())
    }

    /// Return the filesystem path of this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());

        let mut perms = PermissionTracker::new();
        perms
            .create("Read".into(), serde_json::json!({"path": "/tmp"}), None, 0)
            .unwrap();

        let mut elics = ElicitationTracker::new();
        elics.create("Are you sure?".into(), 0);

        store.save(&perms, &elics).await.unwrap();
        let (loaded_perms, loaded_elics) = store.load().await.unwrap();

        assert_eq!(loaded_perms.pending_count(), 1);
        assert_eq!(loaded_elics.pending_count(), 1);
    }

    #[tokio::test]
    async fn remove_clears_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = SessionStore::at(tmp.path().to_path_buf());

        let perms = PermissionTracker::new();
        let elics = ElicitationTracker::new();
        store.save(&perms, &elics).await.unwrap();
        assert!(store.path().exists());

        store.remove().await.unwrap();
        assert!(!store.path().exists());
    }
}
