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
    let path = pid_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    std::fs::write(&path, pid.to_string())
}

/// Remove the PID file.
pub fn remove_pid_file() -> std::io::Result<()> {
    let path = pid_file_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Read the PID from the PID file, if it exists.
#[must_use]
pub fn read_pid() -> Option<u32> {
    let path = pid_file_path();
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Check if a process with the given PID is running.
#[cfg(unix)]
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
pub fn stop_daemon() -> Result<String, String> {
    let pid = read_pid().ok_or("No PID file found. Is the server running?".to_string())?;

    if !is_process_running(pid) {
        let _ = remove_pid_file();
        return Err(format!("Process {pid} is not running. Cleaned up stale PID file."));
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

    // Brief wait for graceful shutdown
    std::thread::sleep(std::time::Duration::from_millis(500));

    if is_process_running(pid) {
        Ok(format!("Sent SIGTERM to process {pid}. Server is shutting down..."))
    } else {
        let _ = remove_pid_file();
        Ok(format!("Process {pid} has stopped. PID file cleaned."))
    }
}

/// Get the status of the running daemon.
#[must_use]
pub fn serve_status() -> String {
    let pid = match read_pid() {
        Some(p) => p,
        None => return "No running daemon (no PID file found).".to_string(),
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
        let _ = remove_pid_file();
        write_pid_file().unwrap();
        let pid = read_pid();
        assert!(pid.is_some());
        assert_eq!(pid.unwrap(), std::process::id());
        remove_pid_file().unwrap();
    }

    #[test]
    fn read_pid_returns_none_when_missing() {
        let _ = remove_pid_file();
        assert!(read_pid().is_none());
    }

    #[test]
    fn remove_pid_file_is_idempotent() {
        remove_pid_file().unwrap();
        remove_pid_file().unwrap(); // No error on second call
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
