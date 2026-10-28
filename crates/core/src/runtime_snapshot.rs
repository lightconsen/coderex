//! A small runtime snapshot for `/heapdump`.
//!
//! This is **not** a heap profile. Rust has no stable in-process heap
//! introspection API, so a real allocation profile needs jemalloc/DHAT or an
//! external profiler attached to the process. What this module can produce
//! honestly — and what the former `/heapdump` claimed but never wrote — is a
//! text snapshot of the process facts a support ticket usually starts with:
//! pid, RSS/virtual size, thread count, open file descriptors, and the
//! platform/runtime version. The output says what it is so nobody reads it as
//! an allocation profile.

/// One process snapshot, rendered as text.
///
/// The format is deliberately plain `key: value` lines with a header, so it
/// can be pasted into a bug report or diffed between two captures.
#[must_use]
pub fn capture() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# CodeRex runtime snapshot (process facts, not a heap profile)"
    );
    let _ = writeln!(out, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(
        out,
        "os: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    let pid = std::process::id();
    let _ = writeln!(out, "pid: {pid}");
    if let Some(facts) = linux_process_facts(pid) {
        out.push_str(&facts);
    } else {
        out.push_str(&portable_process_facts(pid));
    }
    out
}

/// `/proc/<pid>` facts on Linux: the cheapest and most precise source.
#[cfg(target_os = "linux")]
fn linux_process_facts(pid: u32) -> Option<String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut out = String::new();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            out.push_str(&format!("rss: {}\n", rest.trim()));
        } else if let Some(rest) = line.strip_prefix("VmSize:") {
            out.push_str(&format!("virtual: {}\n", rest.trim()));
        } else if let Some(rest) = line.strip_prefix("Threads:") {
            out.push_str(&format!("threads: {}\n", rest.trim()));
        }
    }
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    out.push_str(&format!("open_files: {fds}\n"));
    Some(out)
}

#[cfg(not(target_os = "linux"))]
const fn linux_process_facts(_pid: u32) -> Option<String> {
    None
}

/// The portable fallback: ask `ps`, which every supported Unix ships.
#[cfg(unix)]
fn portable_process_facts(pid: u32) -> String {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=,vsz=,thcount=", "-p", &pid.to_string()])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let mut parts = line.split_whitespace();
            format!(
                "rss_kb: {}\nvirtual_kb: {}\nthreads: {}\n",
                parts.next().unwrap_or("unknown"),
                parts.next().unwrap_or("unknown"),
                parts.next().unwrap_or("unknown")
            )
        }
        _ => "process facts: unavailable (ps did not answer)\n".to_string(),
    }
}

#[cfg(not(unix))]
fn portable_process_facts(_pid: u32) -> String {
    "process facts: unavailable on this platform\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_names_the_process_and_its_kind() {
        let snapshot = capture();
        assert!(
            snapshot.starts_with("# CodeRex runtime snapshot (process facts, not a heap profile)"),
            "{snapshot}"
        );
        assert!(
            snapshot.contains(&format!("pid: {}", std::process::id())),
            "{snapshot}"
        );
        assert!(snapshot.contains("version: "), "{snapshot}");
        assert!(snapshot.contains("os: "), "{snapshot}");
    }

    #[cfg(unix)]
    #[test]
    fn a_snapshot_reports_at_least_one_thread_and_some_memory() {
        let snapshot = capture();
        assert!(
            !snapshot.contains("rss_kb: unknown") && !snapshot.contains("threads: unknown"),
            "the platform source answered nothing: {snapshot}"
        );
    }
}
