//! Git configuration a repository is not allowed to choose for us.
//!
//! A repository can carry configuration that makes `git` run programs: a
//! `core.fsmonitor` hook fires on `git status`, `core.attributesFile` can point
//! at a file whose filters run on diff, `init.templateDir` can land hooks in a
//! clone, and `protocol.*.allow` can re-enable transports that execute commands.
//! None of that is visible in a command line, and all of it matters the moment
//! CodeRex runs `git` inside a checkout it did not create.
//!
//! The reference solves this by injecting a fixed config table into every git
//! process it spawns, through `GIT_CONFIG_*` environment variables rather than
//! by rewriting the command line — which is what makes it apply to the shell's
//! own git calls too, not just to the ones the program makes. This port keeps
//! the table and the mechanism.
//!
//! It is deliberately **not** a sandbox. It removes the ways a repository's
//! config can decide what runs; it does not stop the program from running.

use std::process::Command;

/// Configuration CodeRex refuses to inherit from a repository.
///
/// Ported key for key from the reference's table, including the deliberate
/// `protocol.file.allow=user` — local-path operations (submodules) need it.
pub const HARDENED_CONFIG: &[(&str, &str)] = &[
    // A hook git runs for `status`/`diff`; it executes a program of the repo's
    // choosing.
    ("core.fsmonitor", ""),
    // An external askpass program, which a repo can point anywhere.
    ("core.askPass", ""),
    // Attributes can name filters (clean/smudge) that git executes.
    ("core.attributesFile", "/dev/null"),
    // A command git runs to list alternate refs.
    ("core.alternateRefsCommand", ""),
    // Signing programs: never taken from configuration.
    ("gpg.program", ""),
    ("gpg.openpgp.program", ""),
    ("gpg.ssh.program", ""),
    ("gpg.ssh.defaultKeyCommand", ""),
    ("log.showSignature", "false"),
    ("tag.forceSignAnnotated", "false"),
    ("push.gpgSign", "false"),
    ("merge.verifySignatures", "false"),
    // Transports that execute commands or reach arbitrary paths.
    ("protocol.ext.allow", "never"),
    ("protocol.fd.allow", "never"),
    ("protocol.git.allow", "never"),
    ("protocol.file.allow", "user"),
    // A template directory is a way to land hooks in a fresh clone.
    ("init.templateDir", ""),
];

/// Apply the hardening to a child process.
///
/// Uses `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>`,
/// which git applies on top of every configuration source — repository,
/// global and system — for that process and anything it spawns.
pub fn harden(command: &mut Command) {
    command.env("GIT_CONFIG_COUNT", HARDENED_CONFIG.len().to_string());
    for (index, (key, value)) in HARDENED_CONFIG.iter().enumerate() {
        command.env(format!("GIT_CONFIG_KEY_{index}"), key);
        command.env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
}

/// A hardened `git` command.
///
/// Every place CodeRex runs `git` on its own behalf should start here; the
/// hardening is only as good as the call sites that use it.
#[must_use]
pub fn git_command() -> Command {
    let mut command = Command::new("git");
    harden(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the contract: every key the reference hardens is here, and
    /// the one deliberate exception is kept.
    #[test]
    fn the_table_covers_the_keys_that_can_execute_programs() {
        let keys: Vec<&str> = HARDENED_CONFIG.iter().map(|(key, _)| *key).collect();

        for expected in [
            "core.fsmonitor",
            "core.askPass",
            "core.attributesFile",
            "core.alternateRefsCommand",
            "gpg.program",
            "gpg.openpgp.program",
            "gpg.ssh.program",
            "gpg.ssh.defaultKeyCommand",
            "init.templateDir",
            "protocol.ext.allow",
            "protocol.fd.allow",
            "protocol.git.allow",
            "protocol.file.allow",
        ] {
            assert!(keys.contains(&expected), "{expected} is missing");
        }

        let file = HARDENED_CONFIG
            .iter()
            .find(|(key, _)| *key == "protocol.file.allow")
            .map(|(_, value)| *value);
        assert_eq!(file, Some("user"), "local paths are needed for submodules");
    }

    /// The environment is what git reads, so that is what is asserted.
    #[test]
    fn hardening_writes_the_environment_git_reads() {
        let mut command = Command::new("git");
        harden(&mut command);

        let envs: Vec<(String, String)> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().to_string(),
                    value.map_or_else(String::new, |v| v.to_string_lossy().to_string()),
                )
            })
            .collect();

        assert!(
            envs.iter()
                .any(|(key, value)| key == "GIT_CONFIG_COUNT" && value == &HARDENED_CONFIG.len().to_string())
        );
        assert!(envs.iter().any(|(key, value)| key == "GIT_CONFIG_KEY_0"
            && value == "core.fsmonitor"));
        assert!(envs.iter().any(|(key, value)| key == "GIT_CONFIG_VALUE_0" && value.is_empty()));
        // Every key has a slot.
        assert_eq!(
            envs.iter()
                .filter(|(key, _)| key.starts_with("GIT_CONFIG_KEY_"))
                .count(),
            HARDENED_CONFIG.len()
        );
    }

    /// The point of the whole thing: a repository's `core.fsmonitor` hook runs
    /// on `git status` — unless git is spawned with this hardening.
    ///
    /// The control run is the same repository without hardening, so the test
    /// proves both that the hook is a real hazard and that the table stops it.
    #[cfg(unix)]
    #[test]
    fn a_repositorys_fsmonitor_hook_does_not_run() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("fsmonitor.sh");
        let marker = dir.path().join("the-hook-ran");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch {}\necho /dev/null\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["config", "core.fsmonitor"])
            .arg(&hook)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());

        // Control: without the hardening, the repository's hook runs.
        let _ = Command::new("git")
            .arg("status")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            marker.exists(),
            "the control run did not trigger the hook, so this test proves nothing"
        );
        std::fs::remove_file(&marker).unwrap();

        // Hardened: it does not.
        let _ = git_command()
            .arg("status")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            !marker.exists(),
            "the repository's fsmonitor hook ran despite the hardening"
        );
    }
}
