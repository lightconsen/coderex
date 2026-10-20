//! Update check — compare local version against GitHub releases.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Current compiled version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// GitHub repo owner for update checks.
const REPO_OWNER: &str = "coderex";
/// GitHub repo name for update checks.
const REPO_NAME: &str = "coderex";

/// Check for updates by querying GitHub's latest release API.
///
/// Returns a message if a newer version is available, `None` otherwise.
pub async fn check_for_updates() -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");

    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    else {
        return None;
    };

    let Ok(response) = client
        .get(&url)
        .header("User-Agent", "coderex-cli")
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
    else {
        return None;
    };

    if !response.status().is_success() {
        return None;
    }

    let body: serde_json::Value = match response.json().await {
        Ok(b) => b,
        Err(_) => return None,
    };

    let latest = body.get("tag_name")?.as_str()?;
    let latest = latest.strip_prefix('v').unwrap_or(latest);

    if is_newer(latest, CURRENT_VERSION) {
        Some(format!(
            "New version available: {latest} (current: {CURRENT_VERSION}). Run `coderex update` or visit https://github.com/{REPO_OWNER}/{REPO_NAME}/releases"
        ))
    } else {
        None
    }
}

/// Compare two semantic version strings.
///
/// Returns `true` if `latest` is newer than `current`.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    let latest_parts: Vec<u32> = latest.split('.').filter_map(|s| s.parse().ok()).collect();
    let current_parts: Vec<u32> = current.split('.').filter_map(|s| s.parse().ok()).collect();

    for (l, c) in latest_parts.iter().zip(current_parts.iter()) {
        if l > c {
            return true;
        }
        if l < c {
            return false;
        }
    }

    latest_parts.len() > current_parts.len()
}

/// Check if we should perform an update check (less than 24h since last).
#[must_use]
pub fn should_check(cwd: &Path) -> bool {
    let marker = cwd.join(".coderex/.last_version_check");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    if !marker.exists() {
        return true;
    }

    let Ok(content) = std::fs::read_to_string(&marker) else {
        return true;
    };

    let last_check: u64 = content.trim().parse().unwrap_or(0);
    let elapsed = now.saturating_sub(last_check);
    elapsed >= Duration::from_secs(24 * 3600).as_secs()
}

/// Record that an update check was performed.
pub fn record_check_time(cwd: &Path) {
    let marker = cwd.join(".coderex/.last_version_check");
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .to_string();
    let _ = std::fs::write(&marker, now);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_minor_bump() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn is_newer_patch_bump() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
    }

    #[test]
    fn is_newer_major_bump() {
        assert!(is_newer("1.0.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
    }

    #[test]
    fn is_newer_same_version() {
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn is_newer_extra_parts() {
        // More parts = newer if prefix matches
        assert!(is_newer("0.1.0.1", "0.1.0"));
    }

    #[test]
    fn should_check_with_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(should_check(tmp.path()));
    }

    #[test]
    fn should_check_with_fresh_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".coderex")).unwrap();
        record_check_time(tmp.path());
        assert!(!should_check(tmp.path()));
    }

    #[test]
    fn should_check_with_old_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".coderex")).unwrap();
        // Write an old timestamp
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(2 * 24 * 3600))
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            tmp.path().join(".coderex/.last_version_check"),
            old.to_string(),
        )
        .unwrap();
        assert!(should_check(tmp.path()));
    }

    #[test]
    fn record_check_time_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        record_check_time(tmp.path());
        assert!(tmp.path().join(".coderex/.last_version_check").exists());
    }
}
