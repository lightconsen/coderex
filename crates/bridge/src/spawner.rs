//! Bridge session spawner — worktree creation, session directory preparation, and SSH remote spawning.

use crate::poller::SpawnMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Spawn configuration for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnConfig {
    /// The session ID.
    pub session_id: String,
    /// The target directory or worktree path.
    pub work_dir: PathBuf,
    /// Spawn mode.
    pub mode: SpawnMode,
    /// Git branch for worktree mode.
    pub branch: Option<String>,
}

/// Result of a spawn operation.
#[derive(Debug)]
pub struct SpawnResult {
    /// The session ID.
    pub session_id: String,
    /// The actual working directory.
    pub work_dir: PathBuf,
    /// Whether a worktree was created.
    pub worktree_created: bool,
}

/// Spawns sessions according to the configured mode.
#[derive(Debug)]
pub struct BridgeSessionSpawner {
    /// Base repository root.
    pub repo_root: PathBuf,
    /// Base directory for worktrees.
    pub worktree_base: PathBuf,
    /// Base directory for session data.
    pub session_base: PathBuf,
}

impl BridgeSessionSpawner {
    /// Create a new spawner.
    #[must_use]
    pub fn new(repo_root: PathBuf, worktree_base: PathBuf, session_base: PathBuf) -> Self {
        Self {
            repo_root,
            worktree_base,
            session_base,
        }
    }

    /// Spawn a session with the given config.
    #[allow(clippy::unused_async)]
    pub async fn spawn(&self, config: SpawnConfig) -> Result<SpawnResult, String> {
        match config.mode {
            SpawnMode::SingleSession => self.spawn_single(config).await,
            SpawnMode::SameDir => self.spawn_same_dir(config).await,
            SpawnMode::Worktree => self.spawn_worktree(config).await,
            SpawnMode::Ssh {
                ref host,
                ref remote_path,
                ref identity_file,
            } => {
                let host = host.clone();
                let remote_path = remote_path.clone();
                let identity_file = identity_file.clone();
                self.spawn_ssh(config, &host, &remote_path, identity_file.as_deref())
                    .await
            }
        }
    }

    async fn spawn_single(&self, config: SpawnConfig) -> Result<SpawnResult, String> {
        let work_dir = config.work_dir;
        self.prepare_session_dir(&config.session_id).await?;
        Ok(SpawnResult {
            session_id: config.session_id,
            work_dir,
            worktree_created: false,
        })
    }

    async fn spawn_same_dir(&self, config: SpawnConfig) -> Result<SpawnResult, String> {
        let work_dir = config.work_dir;
        self.prepare_session_dir(&config.session_id).await?;
        Ok(SpawnResult {
            session_id: config.session_id,
            work_dir,
            worktree_created: false,
        })
    }

    async fn spawn_worktree(&self, config: SpawnConfig) -> Result<SpawnResult, String> {
        let branch = config.branch.as_deref().unwrap_or("main");
        let worktree_path = self.worktree_base.join(&config.session_id);

        if !worktree_path.exists() {
            let output = std::process::Command::new("git")
                .args(["worktree", "add", worktree_path.to_str().unwrap(), branch])
                .current_dir(&self.repo_root)
                .output()
                .map_err(|e| format!("git worktree add failed: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "git worktree add failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }

        self.prepare_session_dir(&config.session_id).await?;

        Ok(SpawnResult {
            session_id: config.session_id,
            work_dir: worktree_path,
            worktree_created: true,
        })
    }

    /// Prepare the session data directory.
    async fn prepare_session_dir(&self, session_id: &str) -> Result<(), String> {
        let session_dir = self.session_base.join(session_id);
        std::fs::create_dir_all(&session_dir)
            .map_err(|e| format!("failed to create session dir: {e}"))?;
        Ok(())
    }

    /// Clean up a worktree for the given session.
    pub fn cleanup_worktree(&self, session_id: &str) -> Result<(), String> {
        let worktree_path = self.worktree_base.join(session_id);
        if worktree_path.exists() {
            std::process::Command::new("git")
                .args(["worktree", "remove", worktree_path.to_str().unwrap()])
                .current_dir(&self.repo_root)
                .output()
                .map_err(|e| format!("git worktree remove failed: {e}"))?;
        }
        Ok(())
    }

    /// Spawn a session on a remote host via SSH.
    ///
    /// Connects to the remote host using OpenSSH, creates the session
    /// directory, and verifies that `coderex` is available on the remote.
    async fn spawn_ssh(
        &self,
        config: SpawnConfig,
        host: &str,
        remote_path: &str,
        _identity_file: Option<&str>,
    ) -> Result<SpawnResult, String> {
        let session_dir = format!("{remote_path}/{}", config.session_id);

        // Build the SSH session using the system ssh command.
        // Note: identity_file support requires MuxConfig; for now
        // we rely on the user's ssh config (~/.ssh/config) for key selection.
        let session = openssh::Session::connect(host, openssh::KnownHosts::Accept)
            .await
            .map_err(|e| format!("ssh connect failed: {e}"))?;

        // Create session directory on remote: ssh host mkdir -p /path
        let output = session
            .command("mkdir")
            .arg("-p")
            .arg(&session_dir)
            .output()
            .await
            .map_err(|e| format!("ssh mkdir failed: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "remote mkdir failed: {}",
                String::from_utf8_lossy(&output.stderr),
            ));
        }

        // Verify coderex is available on remote
        let output = session
            .command("which")
            .arg("coderex")
            .output()
            .await
            .map_err(|e| format!("ssh which coderex failed: {e}"))?;

        if !output.status.success() {
            return Err("coderex not found on remote host".to_string());
        }

        Ok(SpawnResult {
            session_id: config.session_id,
            work_dir: PathBuf::from(&session_dir),
            worktree_created: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    fn temp_dirs() -> (PathBuf, PathBuf, PathBuf) {
        let base = tempfile::tempdir().unwrap().keep();
        (
            base.join("repo"),
            base.join("worktrees"),
            base.join("sessions"),
        )
    }

    fn init_git_repo(path: &Path) {
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .output()
            .expect("git init failed");
        std::fs::write(path.join("README"), "root").unwrap();
        std::process::Command::new("git")
            .args(["add", "README"])
            .current_dir(path)
            .output()
            .expect("git add failed");
        std::process::Command::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .current_dir(path)
            .output()
            .expect("git commit failed");
    }

    #[test]
    fn spawn_single_session() {
        let (repo, wt, sess) = temp_dirs();
        std::fs::create_dir_all(&repo).unwrap();
        let spawner = BridgeSessionSpawner::new(repo, wt, sess);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let config = SpawnConfig {
            session_id: "s1".into(),
            work_dir: PathBuf::from("/tmp/work"),
            mode: SpawnMode::SingleSession,
            branch: None,
        };
        let result = rt.block_on(spawner.spawn(config)).unwrap();
        assert!(!result.worktree_created);
        assert_eq!(result.work_dir, PathBuf::from("/tmp/work"));
    }

    #[test]
    fn spawn_worktree_creates_dir() {
        let (repo, wt, sess) = temp_dirs();
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&sess).unwrap();
        let spawner = BridgeSessionSpawner::new(repo, wt, sess);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let config = SpawnConfig {
            session_id: "s1".into(),
            work_dir: PathBuf::from("/tmp"),
            mode: SpawnMode::Worktree,
            branch: Some("HEAD".to_string()),
        };
        let result = rt.block_on(spawner.spawn(config)).unwrap();
        assert!(result.worktree_created);
        assert!(result.work_dir.ends_with("worktrees/s1"));
    }

    #[test]
    fn cleanup_worktree_removes_dir() {
        let (repo, wt, sess) = temp_dirs();
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&sess).unwrap();
        let spawner = BridgeSessionSpawner::new(repo, wt.clone(), sess);
        // Create a worktree first so cleanup has something to remove
        let rt = tokio::runtime::Runtime::new().unwrap();
        let config = SpawnConfig {
            session_id: "s1".into(),
            work_dir: PathBuf::from("/tmp"),
            mode: SpawnMode::Worktree,
            branch: Some("HEAD".to_string()),
        };
        rt.block_on(spawner.spawn(config)).unwrap();
        assert!(wt.join("s1").exists());
        spawner.cleanup_worktree("s1").unwrap();
        assert!(!wt.join("s1").exists());
    }

    #[test]
    fn cleanup_nonexistent_worktree_is_ok() {
        let (repo, wt, sess) = temp_dirs();
        let spawner = BridgeSessionSpawner::new(repo, wt, sess);
        spawner.cleanup_worktree("nonexistent").unwrap();
    }

    #[test]
    fn spawn_mode_ssh_variant() {
        let ssh_mode = SpawnMode::Ssh {
            host: "user@remote".into(),
            remote_path: "/tmp/work".into(),
            identity_file: Some("~/.ssh/id_ed25519".into()),
        };
        match &ssh_mode {
            SpawnMode::Ssh {
                host,
                remote_path,
                identity_file,
            } => {
                assert_eq!(host, "user@remote");
                assert_eq!(remote_path, "/tmp/work");
                assert_eq!(identity_file, &Some("~/.ssh/id_ed25519".into()));
            }
            _ => panic!("expected Ssh variant"),
        }
    }

    #[test]
    fn spawn_mode_ssh_no_identity() {
        let ssh_mode = SpawnMode::Ssh {
            host: "user@remote".into(),
            remote_path: "/tmp/work".into(),
            identity_file: None,
        };
        assert_eq!(ssh_mode.as_str(), "ssh");
    }
}
