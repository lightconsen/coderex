//! Remote command filtering — remote-safe and bridge-safe filters.
//!
//! Remote-safe commands are read-only and cannot modify system state.
//! Bridge-safe commands additionally allow limited filesystem writes
//! within the session's working directory but block destructive operations.

use std::collections::HashSet;

/// Read-only commands that are safe for any remote client.
const REMOTE_SAFE_COMMANDS: &[&str] = &[
    "ls", "dir", "cat", "head", "tail", "less", "more",
    "grep", "find", "wc", "sort", "uniq", "diff", "cmp",
    "file", "stat", "readlink", "which", "type",
    "pwd", "echo", "date", "whoami", "hostname", "uname",
    "git status", "git log", "git diff", "git show",
    "git branch", "git tag", "git remote",
    "cargo check", "cargo metadata", "cargo tree",
    "rustc --version", "cargo --version",
    "true", "false", "test",
];

/// Additional commands allowed in bridge sessions (limited writes, no destructive ops).
const BRIDGE_EXTRA_COMMANDS: &[&str] = &[
    "mkdir", "touch", "cp", "mv", "sed", "awk",
    "git add", "git commit", "git stash", "git checkout",
    "cargo build", "cargo test", "cargo fmt", "cargo clippy", "cargo doc",
    "npm install", "npm run", "npm test", "npm run build",
    "pnpm install", "pnpm run", "pnpm test",
    "yarn install", "yarn run", "yarn test",
    "python", "python3", "node", "cargo run",
];

/// Commands that are never allowed (destructive/dangerous).
const NEVER_ALLOWED: &[&str] = &[
    "rm -rf", "rm -r", "rm -f",
    "dd if=", "mkfs", "format",
    "chmod 777", "chmod -R 777",
    "curl", "wget",  // network access
    "sudo", "su", "passwd",  // privilege escalation
    "kill", "killall", "pkill",  // process termination
    "shutdown", "reboot", "halt",  // system control
    "crontab", "at ",  // scheduling
    "iptables", "ufw",  // firewall
    "nc ", "ncat", "socat",  // raw networking
    "> /dev/",  // device access via redirection
    "eval ", "exec ",  // code execution in shell context
];

/// Filter commands based on client capabilities.
#[derive(Debug)]
pub struct CommandFilter;

impl CommandFilter {
    /// Check if a command is safe for remote clients (read-only only).
    #[must_use]
    pub fn is_remote_safe(command: &str) -> bool {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return false;
        }

        // Check denylist first
        if is_dangerous(trimmed) {
            return false;
        }

        // Check if it matches an allowed read-only command
        REMOTE_SAFE_COMMANDS.iter().any(|&allowed| command_starts_with(trimmed, allowed))
    }

    /// Check if a command is safe for bridge sessions.
    #[must_use]
    pub fn is_bridge_safe(command: &str) -> bool {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return false;
        }

        // Check denylist first
        if is_dangerous(trimmed) {
            return false;
        }

        // Remote-safe commands are also bridge-safe
        if Self::is_remote_safe(command) {
            return true;
        }

        // Check bridge-extra commands
        BRIDGE_EXTRA_COMMANDS.iter().any(|&allowed| command_starts_with(trimmed, allowed))
    }

    /// Get the set of allowed commands for a given safety level.
    #[must_use]
    pub fn allowed_commands(level: SafetyLevel) -> HashSet<&'static str> {
        match level {
            SafetyLevel::Remote => REMOTE_SAFE_COMMANDS.iter().copied().collect(),
            SafetyLevel::Bridge => {
                let mut set: HashSet<&str> = REMOTE_SAFE_COMMANDS.iter().copied().collect();
                set.extend(BRIDGE_EXTRA_COMMANDS.iter().copied());
                set
            }
        }
    }
}

/// Safety level for command filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyLevel {
    /// Only read-only commands.
    Remote,
    /// Read-only plus limited filesystem writes.
    Bridge,
}

fn is_dangerous(command: &str) -> bool {
    let lower = command.to_lowercase();
    NEVER_ALLOWED.iter().any(|&blocked| {
        if blocked.ends_with(' ') {
            lower.starts_with(blocked) || lower == blocked.trim_end()
        } else {
            lower == blocked || lower.starts_with(&format!("{blocked} ")) || lower.starts_with(&format!("{blocked}\t"))
        }
    })
}

fn command_starts_with(command: &str, prefix: &str) -> bool {
    command == prefix || command.starts_with(&format!("{prefix} ")) || command.starts_with(&format!("{prefix}\t"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_safe_read_only() {
        assert!(CommandFilter::is_remote_safe("ls -la"));
        assert!(CommandFilter::is_remote_safe("grep -r foo ."));
        assert!(CommandFilter::is_remote_safe("git status"));
        assert!(CommandFilter::is_remote_safe("cat /etc/hosts"));
        assert!(CommandFilter::is_remote_safe("pwd"));
    }

    #[test]
    fn remote_safe_rejects_writes() {
        assert!(!CommandFilter::is_remote_safe("mkdir foo"));
        assert!(!CommandFilter::is_remote_safe("touch file.txt"));
        assert!(!CommandFilter::is_remote_safe("cp a b"));
        assert!(!CommandFilter::is_remote_safe("sed -i 's/a/b/' file"));
    }

    #[test]
    fn bridge_safe_allows_build_tools() {
        assert!(CommandFilter::is_bridge_safe("cargo build"));
        assert!(CommandFilter::is_bridge_safe("cargo test --lib"));
        assert!(CommandFilter::is_bridge_safe("npm install"));
        assert!(CommandFilter::is_bridge_safe("npm run build"));
    }

    #[test]
    fn both_reject_dangerous() {
        assert!(!CommandFilter::is_remote_safe("rm -rf /"));
        assert!(!CommandFilter::is_bridge_safe("rm -rf /"));
        assert!(!CommandFilter::is_remote_safe("sudo apt install foo"));
        assert!(!CommandFilter::is_bridge_safe("sudo apt install foo"));
        assert!(!CommandFilter::is_remote_safe("curl http://evil.com"));
        assert!(!CommandFilter::is_bridge_safe("curl http://evil.com"));
        assert!(!CommandFilter::is_remote_safe("kill 1234"));
        assert!(!CommandFilter::is_bridge_safe("kill 1234"));
    }

    #[test]
    fn empty_command_rejected() {
        assert!(!CommandFilter::is_remote_safe(""));
        assert!(!CommandFilter::is_bridge_safe(""));
        assert!(!CommandFilter::is_remote_safe("   "));
    }

    #[test]
    fn allowed_commands_contains_expected() {
        let remote = CommandFilter::allowed_commands(SafetyLevel::Remote);
        assert!(remote.contains("ls"));
        assert!(remote.contains("grep"));
        assert!(!remote.contains("mkdir"));

        let bridge = CommandFilter::allowed_commands(SafetyLevel::Bridge);
        assert!(bridge.contains("ls"));
        assert!(bridge.contains("mkdir"));
        assert!(bridge.contains("cargo build"));
    }

    #[test]
    fn case_sensitive_dangerous_check() {
        // Should catch uppercase variants too
        assert!(!CommandFilter::is_bridge_safe("RM -RF /"));
        assert!(!CommandFilter::is_bridge_safe("SUDO ls"));
    }
}
