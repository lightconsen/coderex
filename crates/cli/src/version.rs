//! Update check — compare local version against GitHub releases.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Current compiled version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// GitHub repo owner for update checks.
pub const REPO_OWNER: &str = "lightconsen";
/// GitHub repo name for update checks.
pub const REPO_NAME: &str = "coderex";

/// Set this to stop the once-a-day startup check (air-gapped machines, or a
/// host that wants no outbound requests it did not ask for).
pub const DISABLE_UPDATE_CHECK_ENV: &str = "CODEREX_DISABLE_UPDATE_CHECK";

/// The releases page, for messages that tell the user where to look.
#[must_use]
pub fn releases_url() -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases")
}

/// Check for updates by querying GitHub's latest release API.
///
/// Returns a message if a newer version is available, `None` otherwise.
pub async fn check_for_updates() -> Option<String> {
    let release = crate::self_update::latest_release(Duration::from_secs(5))
        .await
        .ok()?;

    if is_newer(&release.version, CURRENT_VERSION) {
        Some(format!(
            "New version available: {} (current: {CURRENT_VERSION}). Run `coderex update` or visit {}",
            release.version,
            releases_url()
        ))
    } else {
        None
    }
}

/// Compare two semantic version strings.
///
/// Returns `true` if `latest` is newer than `current`. Prerelease and build
/// metadata follow SemVer (`0.2.0-rc.1` sorts before `0.2.0`); tags that do
/// not parse as a version fall back to comparing dot-separated numbers, which
/// is what this used to do for everything.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        // `cmp_precedence`, not `>`: build metadata is part of `Ord` but not
        // of SemVer precedence, so `0.2.0+build.7` is the same release as
        // `0.2.0` and must not look newer.
        (Ok(latest), Ok(current)) => latest.cmp_precedence(&current) == std::cmp::Ordering::Greater,
        _ => dot_parts_newer(latest, current),
    }
}

/// The pre-semver comparison: more or larger dot-separated numbers wins.
fn dot_parts_newer(latest: &str, current: &str) -> bool {
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

/// Whether the given `CODEREX_DISABLE_UPDATE_CHECK` value turns checks off.
///
/// Pure so the answer can be tested without mutating the process environment.
#[must_use]
pub fn update_check_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Check if we should perform an update check (less than 24h since last).
#[must_use]
pub fn should_check(cwd: &Path) -> bool {
    let disabled = std::env::var(DISABLE_UPDATE_CHECK_ENV).ok();
    if update_check_disabled(disabled.as_deref()) {
        return false;
    }
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
    elapsed >= Duration::from_hours(24).as_secs()
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
    fn is_newer_orders_a_prerelease_before_its_release() {
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
    }

    #[test]
    fn is_newer_ignores_build_metadata() {
        assert!(!is_newer("0.2.0+build.7", "0.2.0"));
    }

    #[test]
    fn the_update_check_switch_accepts_the_usual_truthy_values() {
        for value in ["1", "true", "YES", " on "] {
            assert!(update_check_disabled(Some(value)), "{value}");
        }
        for value in ["0", "false", "no", ""] {
            assert!(!update_check_disabled(Some(value)), "{value}");
        }
        assert!(!update_check_disabled(None));
    }

    #[test]
    fn the_releases_url_names_the_public_repo() {
        assert_eq!(
            releases_url(),
            "https://github.com/lightconsen/coderex/releases"
        );
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
            .checked_sub(Duration::from_hours(48))
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
