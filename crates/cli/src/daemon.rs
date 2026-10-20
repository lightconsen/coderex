//! Daemon PID file management for serve/serve-stop/serve-status.
#![allow(unsafe_code)]

use std::path::PathBuf;

/// Path to the daemon PID file.
#[must_use]
pub fn pid_file_path() -> PathBuf {
    let mut path = coderex_config::config_home();
    path.push("daemon.pid");
    path
}

/// Write the current process PID to the PID file.
pub fn write_pid_file() -> std::io::Result<()> {
    write_pid_file_at(&pid_file_path())
}

/// Remove the PID file.
pub fn remove_pid_file() -> std::io::Result<()> {
    remove_pid_file_at(&pid_file_path())
}

/// Read the PID from the PID file, if it exists.
#[must_use]
pub fn read_pid() -> Option<u32> {
    read_pid_at(&pid_file_path())
}

/// Write the current process PID to `path`.
///
/// Split out so the tests can use a directory of their own: the tests used to
/// share the developer's real `~/.coderex/daemon.pid`, and running in parallel
/// meant one test deleting the file another was reading.
fn write_pid_file_at(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string())
}

fn remove_pid_file_at(path: &std::path::Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn read_pid_at(path: &std::path::Path) -> Option<u32> {
    let content = std::fs::read_to_string(path).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Check if a process with the given PID is running.
// PIDs fit in i32 on all supported platforms; the cast cannot wrap in practice.
#[cfg(unix)]
#[allow(clippy::cast_possible_wrap)]
pub fn is_process_running(pid: u32) -> bool {
    // SAFETY: kill with sig=0 checks if process exists, doesn't send a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_process_running(_pid: u32) -> bool {
    // On Windows, no reliable way without WinAPI; assume running if file exists
    true
}

/// Stop the daemon by sending SIGTERM to the running process.
// PIDs fit in i32 on all supported platforms; the cast cannot wrap in practice.
#[allow(clippy::cast_possible_wrap)]
pub fn stop_daemon() -> Result<String, String> {
    let pid = read_pid().ok_or_else(|| "No PID file found. Is the server running?".to_string())?;

    if !is_process_running(pid) {
        let _ = remove_pid_file();
        return Err(format!(
            "Process {pid} is not running. Cleaned up stale PID file."
        ));
    }

    #[cfg(unix)]
    {
        // SAFETY: sending SIGTERM to a known PID
        let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if result != 0 {
            return Err(format!("Failed to send SIGTERM to process {pid}"));
        }
    }

    #[cfg(not(unix))]
    {
        return Err("Stopping daemon is only supported on Unix systems".to_string());
    }

    #[cfg(unix)]
    {
        // Brief wait for graceful shutdown
        std::thread::sleep(std::time::Duration::from_millis(500));

        if is_process_running(pid) {
            Ok(format!(
                "Sent SIGTERM to process {pid}. Server is shutting down..."
            ))
        } else {
            let _ = remove_pid_file();
            Ok(format!("Process {pid} has stopped. PID file cleaned."))
        }
    }
}

/// Get the status of the running daemon.
#[must_use]
pub fn serve_status() -> String {
    let Some(pid) = read_pid() else {
        return "No running daemon (no PID file found).".to_string();
    };

    if is_process_running(pid) {
        format!("Daemon is running (PID: {pid}).")
    } else {
        let _ = remove_pid_file();
        format!("Stale PID file found (PID: {pid}), but process is not running. Cleaned up.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_path_is_correct() {
        let path = pid_file_path();
        assert!(path.ends_with("daemon.pid"));
    }

    #[test]
    fn write_and_read_pid_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        write_pid_file_at(&path).unwrap();
        let pid = read_pid_at(&path);
        assert!(pid.is_some());
        assert_eq!(pid.unwrap(), std::process::id());
        remove_pid_file_at(&path).unwrap();
    }

    #[test]
    fn read_pid_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pid_at(&dir.path().join("daemon.pid")).is_none());
    }

    #[test]
    fn remove_pid_file_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        remove_pid_file_at(&path).unwrap();
        remove_pid_file_at(&path).unwrap(); // No error on second call
    }

    #[test]
    fn current_process_is_running() {
        assert!(is_process_running(std::process::id()));
    }

    #[test]
    fn stop_daemon_no_pid_file() {
        let _ = remove_pid_file();
        let result = stop_daemon();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No PID file"));
    }
}
