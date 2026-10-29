//! Spawn a bridge session on a real SSH host.
//!
//! The spawner's unit tests cover the mapping and the command list; this test
//! covers what only a live server can: the connection, the identity file, the
//! remote `mkdir -p`, and the remote `which coderex`.
//!
//! It runs when `CODEREX_SSH_TEST_HOST` names a reachable host. CI's
//! `ssh-integration` job starts a throwaway `sshd` on localhost and sets the
//! three variables; without them the test skips, because a developer machine
//! is not expected to have an SSH server running for a unit test.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `CODEREX_SSH_TEST_HOST` | `user@host:port` to connect to |
//! | `CODEREX_SSH_TEST_KEY` | private key file (optional; defaults to the agent) |
//! | `CODEREX_SSH_TEST_PATH` | remote directory to create the session under (default `/tmp/coderex-ssh-test`) |

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use coderex_bridge::poller::SpawnMode;
use coderex_bridge::spawner::{BridgeSessionSpawner, SpawnConfig};

#[tokio::test]
async fn a_session_is_spawned_on_a_real_ssh_host() {
    let Some(host) = std::env::var("CODEREX_SSH_TEST_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        eprintln!("skipping: CODEREX_SSH_TEST_HOST is not set");
        return;
    };
    let identity = std::env::var("CODEREX_SSH_TEST_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let remote_path = std::env::var("CODEREX_SSH_TEST_PATH")
        .unwrap_or_else(|_| "/tmp/coderex-ssh-test".to_string());

    let spawner = BridgeSessionSpawner::new(
        PathBuf::from("/nonexistent/repo"),
        PathBuf::from("/nonexistent/worktrees"),
        PathBuf::from("/nonexistent/sessions"),
    );
    let session_id = format!("sess-{}", std::process::id());

    let result = spawner
        .spawn(SpawnConfig {
            session_id: session_id.clone(),
            // Unused by the SSH path: the session lives on the remote.
            work_dir: PathBuf::new(),
            mode: SpawnMode::Ssh {
                host: host.clone(),
                remote_path: remote_path.clone(),
                identity_file: identity.clone(),
            },
            branch: None,
        })
        .await;

    let cleanup = Cleanup {
        host,
        identity,
        path: format!("{remote_path}/{session_id}"),
    };
    let result = result.inspect_err(|error| {
        eprintln!("ssh spawn failed: {error}");
    });
    let spawned = result.expect("the SSH session should spawn");
    cleanup.remove_remote_dir();

    assert_eq!(
        spawned.work_dir,
        PathBuf::from(format!("{remote_path}/{session_id}")),
        "the session directory is the remote path the spawner reported"
    );
}

/// Best effort: remove the directory the test created on the remote.
struct Cleanup {
    host: String,
    identity: Option<String>,
    path: String,
}

impl Cleanup {
    fn remove_remote_dir(&self) {
        let (target, port) = split_port(&self.host);
        let mut command = Command::new("ssh");
        command
            .args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no"])
            .args(["-o", "UserKnownHostsFile=/dev/null"]);
        if let Some(port) = port {
            command.args(["-p", &port.to_string()]);
        }
        if let Some(identity) = &self.identity {
            command.args(["-i", identity]);
        }
        let _ = command
            .arg(&target)
            .arg(format!("rm -rf {}", self.path))
            .status();
    }
}

fn split_port(host: &str) -> (String, Option<u16>) {
    match host.rsplit_once(':') {
        Some((target, port)) => match port.parse::<u16>() {
            Ok(port) if !target.contains(':') => (target.to_string(), Some(port)),
            _ => (host.to_string(), None),
        },
        None => (host.to_string(), None),
    }
}
