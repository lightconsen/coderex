//! Real tmux integration for `--tmux`.
//!
//! Two behaviours, one flag:
//!
//! * **Outside tmux** — start a new tmux session whose only command is this
//!   binary with the same arguments (minus `--tmux`). The session *is* the
//!   process; when it ends, this process exits with the same status.
//! * **Inside tmux** — do not nest a session. Set the window and pane titles
//!   and carry on in the pane that already exists.
//!
//! Re-exec loops are prevented twice over: `$TMUX` marks the child as already
//! inside tmux, and `CODEREX_TMUX_ACTIVE` is set in the child's environment so
//! a stale or unset `$TMUX` cannot cause another wrap.

use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Set in the re-executed child, so it never wraps itself again.
pub const TMUX_CHILD_ENV: &str = "CODEREX_TMUX_ACTIVE";

/// The title applied to the window and pane when already inside tmux.
pub const WINDOW_TITLE: &str = "coderex";

/// Apply `--tmux`, if set.
///
/// # Errors
///
/// Fails when `--tmux` is set outside a terminal, when `tmux` is not on
/// `PATH`, when the current executable cannot be resolved, or when tmux
/// itself refuses to start. Never returns after a successful re-launch: the
/// process exits with the tmux client's status.
pub fn apply(enabled: bool) -> Result<()> {
    if !enabled {
        return Ok(());
    }

    if inside_tmux() {
        set_titles()?;
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        bail!(
            "--tmux starts a tmux session, which needs an interactive terminal; \
             stdin is not a terminal here"
        );
    }

    let Some(tmux) = find_executable("tmux") else {
        bail!("--tmux requires the `tmux` executable on PATH; install tmux or omit --tmux");
    };

    let program = std::env::current_exe()
        .context("could not resolve the coderex executable to re-launch inside tmux")?;
    let cwd = std::env::current_dir().context("could not resolve the working directory")?;
    let args = strip_tmux_flag(std::env::args_os().skip(1));
    let plan = RelaunchPlan::new(tmux, program, args, cwd, std::process::id());

    let status = plan
        .command()
        .status()
        .context("could not start a tmux session")?;
    // The tmux client returned, so the session is over. Mirror its status.
    std::process::exit(status.code().unwrap_or(1));
}

/// Whether this process is already running inside a tmux pane.
#[must_use]
pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
        || std::env::var_os(TMUX_CHILD_ENV).is_some()
}

/// Whether `--tmux` should re-launch rather than just retitle.
#[must_use]
pub const fn needs_relaunch(enabled: bool, inside: bool) -> bool {
    enabled && !inside
}

/// Remove `--tmux` (and `--tmux=true` spellings) from a set of arguments.
///
/// The flag is removed before the child runs, so even without the environment
/// marker the child would have nothing to act on.
#[must_use]
pub fn strip_tmux_flag<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    args.into_iter()
        .filter(|arg| {
            let Some(text) = arg.to_str() else {
                return true;
            };
            text != "--tmux" && !text.starts_with("--tmux=")
        })
        .collect()
}

/// The command that runs this binary inside a fresh tmux session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaunchPlan {
    /// Path to the tmux client.
    pub tmux: PathBuf,
    /// Path to the binary to run in the pane.
    pub program: PathBuf,
    /// Arguments for the binary, with `--tmux` already removed.
    pub args: Vec<OsString>,
    /// Working directory for the session.
    pub cwd: PathBuf,
    /// Session name; unique per process so concurrent runs do not collide.
    pub session_name: String,
}

impl RelaunchPlan {
    /// Build a plan for one process.
    #[must_use]
    pub fn new(
        tmux: PathBuf,
        program: PathBuf,
        args: Vec<OsString>,
        cwd: PathBuf,
        pid: u32,
    ) -> Self {
        Self {
            tmux,
            program,
            args: strip_tmux_flag(args),
            cwd,
            session_name: format!("coderex-{pid}"),
        }
    }

    /// The argv passed to tmux, starting with `new-session`.
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec![
            OsString::from("new-session"),
            OsString::from("-s"),
            OsString::from(&self.session_name),
            OsString::from("-n"),
            OsString::from(WINDOW_TITLE),
            OsString::from("-c"),
            self.cwd.as_os_str().to_os_string(),
            self.program.as_os_str().to_os_string(),
        ];
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Build the executable command, including the loop sentinel.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.tmux);
        command.args(self.argv());
        command.env(TMUX_CHILD_ENV, "1");
        command
    }
}

/// Set the window and pane titles of the current tmux pane.
///
/// `rename-window` must succeed; the other two are best-effort because older
/// tmux builds may not accept them, and a title is not worth refusing to
/// start a session over.
///
/// # Errors
///
/// Fails when no `tmux` executable is available or `rename-window` fails.
pub fn set_titles() -> Result<()> {
    let Some(tmux) = find_executable("tmux") else {
        bail!(
            "--tmux is active (TMUX is set) but the `tmux` executable is not on PATH; \
             cannot set the window title"
        );
    };
    let title = title_for(&std::env::current_dir().unwrap_or_default());
    set_titles_with(&tmux, &title)
}

fn set_titles_with(tmux: &Path, title: &str) -> Result<()> {
    let rename = Command::new(tmux)
        .args(["rename-window", title])
        .status()
        .context("could not run `tmux rename-window`")?;
    if !rename.success() {
        bail!("`tmux rename-window` failed with {rename}");
    }

    // Keep tmux from renaming the window back to the running command.
    let _ = Command::new(tmux)
        .args(["set-window-option", "automatic-rename", "off"])
        .status();
    let _ = Command::new(tmux)
        .args(["select-pane", "-T", title])
        .status();
    Ok(())
}

fn title_for(cwd: &Path) -> String {
    match cwd.file_name().and_then(OsStr::to_str) {
        Some(name) if !name.is_empty() => format!("{WINDOW_TITLE}: {name}"),
        _ => WINDOW_TITLE.to_string(),
    }
}

/// Resolve an executable on `PATH`.
#[must_use]
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{name}.exe"));
            if is_executable(&with_exe) {
                return Some(with_exe);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(all(test, unix))]
#[allow(unsafe_code)] // PATH mutation is unsafe in edition 2024; ENV_LOCK guards it.
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn strip_removes_every_spelling_of_the_flag() {
        let args = vec![
            OsString::from("--tmux"),
            OsString::from("--model"),
            OsString::from("sonnet"),
            OsString::from("--tmux=true"),
            OsString::from("--tmux-other"),
        ];
        let stripped = strip_tmux_flag(args);
        assert_eq!(
            stripped,
            vec![
                OsString::from("--model"),
                OsString::from("sonnet"),
                OsString::from("--tmux-other")
            ]
        );
    }

    #[test]
    fn relaunch_argv_runs_the_program_without_the_flag() {
        let plan = RelaunchPlan::new(
            PathBuf::from("/usr/bin/tmux"),
            PathBuf::from("/usr/local/bin/coderex"),
            vec![
                OsString::from("--tmux"),
                OsString::from("--model"),
                OsString::from("sonnet"),
            ],
            PathBuf::from("/work"),
            77,
        );
        let argv = plan.argv();
        assert_eq!(argv[0], OsString::from("new-session"));
        assert!(argv.contains(&OsString::from("coderex-77")));
        assert!(argv.contains(&OsString::from("/usr/local/bin/coderex")));
        assert!(!argv.contains(&OsString::from("--tmux")));
        assert!(argv.contains(&OsString::from("sonnet")));
    }

    #[test]
    fn relaunch_is_skipped_when_already_inside_tmux() {
        assert!(needs_relaunch(true, false));
        assert!(!needs_relaunch(true, true), "must not double-wrap");
        assert!(!needs_relaunch(false, false));
    }

    #[test]
    fn title_uses_the_directory_name() {
        assert_eq!(title_for(Path::new("/work/project")), "coderex: project");
        assert_eq!(title_for(Path::new("/")), WINDOW_TITLE);
    }

    /// Mock `tmux` on PATH: it records its argv so the test can prove the
    /// child command has no `--tmux`, and that retitling uses the real verbs.
    #[test]
    fn a_mock_tmux_on_path_receives_the_expected_commands() {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("tmux.log");
        let tmux = dir.path().join("tmux");
        std::fs::write(
            &tmux,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&tmux).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmux, perms).unwrap();

        let previous = std::env::var_os("PATH");
        // SAFETY: `ENV_LOCK` is held for this test.
        unsafe { std::env::set_var("PATH", dir.path()) };

        let plan = RelaunchPlan::new(
            tmux.clone(),
            PathBuf::from("/bin/true"),
            vec![OsString::from("--tmux"), OsString::from("--verbose")],
            PathBuf::from("/work"),
            9,
        );
        let status = plan.command().status().unwrap();
        assert!(status.success());
        set_titles_with(&tmux, "coderex: work").unwrap();

        // SAFETY: as above.
        unsafe {
            match previous {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }

        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(
            recorded.contains("new-session -s coderex-9"),
            "tmux was not asked to start a session: {recorded}"
        );
        assert!(
            recorded.contains("/bin/true --verbose"),
            "the child command is wrong: {recorded}"
        );
        assert!(
            !recorded.contains("--tmux"),
            "the flag was passed through and would wrap again: {recorded}"
        );
        assert!(
            recorded.contains("rename-window coderex: work"),
            "window title was not set: {recorded}"
        );
    }
}
