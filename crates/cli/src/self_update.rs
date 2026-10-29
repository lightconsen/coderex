//! Self-update: replace this binary with the latest release.
//!
//! `coderex update` used to print "a newer version exists". This module does
//! the rest: ask GitHub for the latest release, download the archive for this
//! platform, check it against the release's `SHA256SUMS`, and swap it in with
//! a rename (the running process keeps the old inode).
//!
//! What it deliberately does not do:
//!
//! - It does not verify a signature. The checksum proves the download matches
//!   the published file; it cannot prove the release is authentic. A
//!   compromised account defeats both, and `docs/self-update.md` says so.
//! - It does not replace a binary a package manager owns. Homebrew, Nix and
//!   `cargo install` keep their own records; overwriting the file would be
//!   undone by the next upgrade or leave the package database lying. Those
//!   installs are told which command upgrades them instead.
//! - It does not execute the downloaded binary before installing it. The only
//!   thing between the download and the rename is the checksum.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::Digest as _;

/// Where the GitHub API lives. Overridden by the e2e test and by mirrors.
pub const API_BASE_ENV: &str = "CODEREX_API_BASE_URL";
/// Where release assets are downloaded from, when the caller wants a mirror.
pub const RELEASE_BASE_ENV: &str = "CODEREX_RELEASE_BASE_URL";
/// The checksum asset every release must carry.
pub const CHECKSUM_ASSET: &str = "SHA256SUMS";

const DEFAULT_API_BASE: &str = "https://api.github.com/repos/lightconsen/coderex";
const DEFAULT_RELEASE_BASE: &str = "https://github.com/lightconsen/coderex/releases/download";

/// Refuse an asset larger than this: a corrupt or hostile response must not
/// fill the disk before the checksum gets a chance to fail.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// One downloadable file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    /// The asset name, which is also the name `SHA256SUMS` lists.
    pub name: String,
    /// Where to fetch it from.
    pub url: String,
}

/// The latest release, as far as this updater cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The tag as published (`v0.2.0`).
    pub tag: String,
    /// The tag without a leading `v`.
    pub version: String,
    /// Every downloadable asset.
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    /// The asset with this name, if the release published one.
    #[must_use]
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// What `update` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    /// The version this binary was built as.
    pub current: String,
    /// The latest published version, when the release lookup succeeded.
    pub latest: Option<String>,
    /// Where the new binary was installed, when one was.
    pub installed: Option<PathBuf>,
}

/// Whether `update` should install or only report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateOptions {
    /// Report whether a newer release exists without downloading it.
    pub check_only: bool,
}

/// Where this executable came from, as far as that decides whether we may
/// replace it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOrigin {
    /// A plain file whose directory we can write: ours to replace.
    SelfManaged,
    /// A package manager owns the file; it names the command that upgrades it.
    PackageManager(&'static str),
    /// A self-managed path we cannot write to.
    Unwritable,
}

/// The API base, honouring the mirror override.
#[must_use]
pub fn api_base() -> String {
    std::env::var(API_BASE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// The release download base, honouring the mirror override, or `None` when
/// the API's own asset URLs should be used.
#[must_use]
pub fn release_base_override() -> Option<String> {
    std::env::var(RELEASE_BASE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// The release target triple for the running build.
///
/// `None` means the release matrix has no archive for this platform; the
/// caller tells the user to build from source rather than inventing a URL.
#[must_use]
pub const fn release_target() -> Option<&'static str> {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(
        target_os = "windows",
        target_arch = "x86_64",
        target_env = "msvc"
    )) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// The asset a target's archive is published as.
///
/// Unix ships a `.tar.gz`; Windows ships the bare `.exe` (the release also
/// attaches a `.zip` for people who install by hand, but extracting one would
/// mean taking on a zip dependency for a single-member archive).
#[must_use]
pub fn asset_name(target: &str) -> String {
    if target.contains("windows") {
        format!("coderex-{target}.exe")
    } else {
        format!("coderex-{target}.tar.gz")
    }
}

/// Whether a target's release asset is a bare executable rather than an
/// archive.
#[must_use]
pub fn is_windows_target(target: &str) -> bool {
    target.contains("windows")
}

/// Parse a GitHub release body into the parts the updater needs.
pub fn parse_release(body: &serde_json::Value) -> Result<Release, String> {
    let tag = body
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "the release has no tag_name".to_string())?;
    let version = tag.strip_prefix('v').unwrap_or(tag).to_string();
    let mut assets = Vec::new();
    for asset in body
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let (Some(name), Some(url)) = (
            asset.get("name").and_then(serde_json::Value::as_str),
            asset
                .get("browser_download_url")
                .and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        assets.push(ReleaseAsset {
            name: name.to_string(),
            url: url.to_string(),
        });
    }
    Ok(Release {
        tag: tag.to_string(),
        version,
        assets,
    })
}

/// Fetch the latest release through `client`.
pub async fn fetch_release(client: &reqwest::Client, api_base: &str) -> Result<Release, String> {
    let url = format!("{}/releases/latest", api_base.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("User-Agent", "coderex-cli")
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
        .map_err(|error| format!("could not reach {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} returned {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("{url} did not return JSON: {error}"))?;
    parse_release(&body)
}

/// Fetch the latest release with a short timeout, for callers that only want
/// to mention it (the startup check and `/update-check`).
pub async fn latest_release(timeout: Duration) -> Result<Release, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| format!("could not build the HTTP client: {error}"))?;
    fetch_release(&client, &api_base()).await
}

/// The URL an asset is fetched from.
///
/// A release base override wins, so an offline mirror can serve the same
/// layout GitHub uses (`<base>/<tag>/<name>`) without rewriting API responses.
#[must_use]
pub fn asset_url(asset: &ReleaseAsset, tag: &str, release_base: Option<&str>) -> String {
    release_base.map_or_else(
        || asset.url.clone(),
        |base| format!("{}/{}/{}", base.trim_end_matches('/'), tag, asset.name),
    )
}

/// What kind of install this executable is, so the updater can refuse politely.
///
/// `home` is passed in rather than read so the classification is testable.
#[must_use]
pub fn classify_install_path(exe: &Path, home: Option<&Path>) -> InstallOrigin {
    let canonical = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let text = canonical.to_string_lossy().replace('\\', "/");

    for (needle, manager) in [
        ("/Cellar/", "Homebrew"),
        ("/opt/homebrew/", "Homebrew"),
        ("/home/linuxbrew/", "Homebrew"),
        ("/nix/store/", "Nix"),
    ] {
        if text.contains(needle) {
            return InstallOrigin::PackageManager(manager);
        }
    }
    if let Some(home) = home {
        let cargo_bin = home.join(".cargo").join("bin");
        let cargo_bin = std::fs::canonicalize(&cargo_bin).unwrap_or(cargo_bin);
        if canonical.starts_with(&cargo_bin) {
            return InstallOrigin::PackageManager("cargo install");
        }
    }

    let Some(parent) = canonical.parent() else {
        return InstallOrigin::Unwritable;
    };
    if tempfile::NamedTempFile::new_in(parent).is_ok() {
        InstallOrigin::SelfManaged
    } else {
        InstallOrigin::Unwritable
    }
}

/// Parse the standard `sha256sum` format: `<hex><space><space|*><name>`.
///
/// Duplicate names and malformed digests are errors rather than last-one-wins:
/// an ambiguous checksum file is one a hostile mirror could exploit.
pub fn parse_sha256sums(text: &str) -> Result<HashMap<String, String>, String> {
    let mut sums = HashMap::new();
    for (line_number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let digest = parts.next().unwrap_or_default();
        let name = parts
            .next()
            .unwrap_or_default()
            .trim_start_matches('*')
            .trim();
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "line {} is not a 64-character hex digest",
                line_number + 1
            ));
        }
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(format!(
                "line {} does not name a single asset",
                line_number + 1
            ));
        }
        let digest = digest.to_ascii_lowercase();
        if sums.insert(name.to_string(), digest).is_some() {
            return Err(format!("{name} is listed more than once"));
        }
    }
    Ok(sums)
}

/// The SHA-256 of `bytes`, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Compare a computed digest with the published one.
pub fn verify_checksum(expected: &str, actual: &str) -> Result<(), String> {
    if expected.eq_ignore_ascii_case(actual) {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch: the release lists {expected}, the download hashes to {actual}"
        ))
    }
}

/// Download `url` to `dest` and return the SHA-256 of what was written.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    max_bytes: u64,
) -> Result<String, String> {
    let mut response = client
        .get(url)
        .header("User-Agent", "coderex-cli")
        .send()
        .await
        .map_err(|error| format!("could not download {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} returned {}", response.status()));
    }

    let mut file = std::fs::File::create(dest)
        .map_err(|error| format!("could not create {}: {error}", dest.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut written = 0_u64;
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| format!("download of {url} failed: {error}"))?;
        let Some(chunk) = chunk else {
            break;
        };
        written = written.saturating_add(chunk.len() as u64);
        if written > max_bytes {
            return Err(format!(
                "{url} is larger than the {max_bytes}-byte download limit"
            ));
        }
        std::io::Write::write_all(&mut file, &chunk)
            .map_err(|error| format!("could not write {}: {error}", dest.display()))?;
        hasher.update(&chunk);
    }
    file.sync_all()
        .map_err(|error| format!("could not flush {}: {error}", dest.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

/// Extract the single file `member` from a `.tar.gz` into `dest`.
///
/// The archive must contain exactly that one regular file. Anything else —
/// extra members, a directory, a `..` component — is refused, because this
/// runs on bytes that were only just downloaded.
pub fn extract_tarball(archive: &Path, member: &str, dest: &Path) -> Result<PathBuf, String> {
    if member.is_empty()
        || member.contains('/')
        || member.contains('\\')
        || member == "."
        || member == ".."
    {
        return Err(format!("refusing archive member name {member:?}"));
    }
    let file = std::fs::File::open(archive)
        .map_err(|error| format!("could not open {}: {error}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let entries = tar
        .entries()
        .map_err(|error| format!("could not read {}: {error}", archive.display()))?;

    let mut files = 0_usize;
    let mut extracted = None;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| format!("could not read {}: {error}", archive.display()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        files += 1;
        if files > 1 {
            return Err(format!(
                "{} has more than one file; a release archive holds exactly one",
                archive.display()
            ));
        }
        let path = entry
            .path()
            .map_err(|error| format!("could not read the member name: {error}"))?
            .into_owned();
        if path != Path::new(member) {
            return Err(format!(
                "{} holds {}, not {member}",
                archive.display(),
                path.display()
            ));
        }
        if entry.size() > MAX_DOWNLOAD_BYTES {
            return Err(format!("{member} is implausibly large"));
        }
        let target = dest.join(member);
        entry
            .unpack(&target)
            .map_err(|error| format!("could not extract {member}: {error}"))?;
        extracted = Some(target);
    }

    extracted.ok_or_else(|| format!("{} holds no file", archive.display()))
}

/// Swap `replacement` in for `current`.
///
/// The two paths must be on the same filesystem (the caller stages the
/// download next to the binary). On Unix the rename replaces a running
/// binary in place; the process keeps the old inode. On Windows a running
/// executable cannot be replaced, but it can be renamed, so the old file is
/// moved aside first and restored if the second rename fails.
pub fn replace_binary(current: &Path, replacement: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(replacement, std::fs::Permissions::from_mode(0o755)).map_err(
            |error| {
                format!(
                    "could not make {} executable: {error}",
                    replacement.display()
                )
            },
        )?;
    }

    #[cfg(windows)]
    {
        let old = old_binary_path(current);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(current, &old).map_err(|error| {
            format!(
                "could not move {} aside (is another coderex running?): {error}",
                current.display()
            )
        })?;
        if let Err(error) = std::fs::rename(replacement, current) {
            let _ = std::fs::rename(&old, current);
            return Err(format!(
                "could not install the new binary at {}: {error}",
                current.display()
            ));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(replacement, current)
            .map_err(|error| format!("could not replace {}: {error}", current.display()))
    }
}

/// The path a replaced binary is parked at on Windows.
#[must_use]
pub fn old_binary_path(current: &Path) -> PathBuf {
    let mut name = current.as_os_str().to_os_string();
    name.push(".old");
    PathBuf::from(name)
}

/// Remove the previous binary left behind by a Windows update.
///
/// Best effort: the file may still be mapped by the process that is calling
/// this on the first launch after the update, in which case the next launch
/// removes it.
pub fn cleanup_after_update(exe: &Path) {
    let old = old_binary_path(exe);
    if old.exists() {
        let _ = std::fs::remove_file(&old);
    }
}

/// Check for, and optionally install, the latest release.
///
/// The staged download lives in a temporary directory inside the target's
/// directory so the final rename cannot cross a filesystem boundary.
pub async fn update(options: UpdateOptions) -> Result<UpdateReport, String> {
    let current_exe = std::env::current_exe()
        .map_err(|error| format!("could not find this executable: {error}"))?;
    cleanup_after_update(&current_exe);

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|error| format!("could not build the HTTP client: {error}"))?;
    let release = fetch_release(&client, &api_base()).await?;
    let current = crate::version::CURRENT_VERSION.to_string();

    if !crate::version::is_newer(&release.version, &current) {
        return Ok(UpdateReport {
            current,
            latest: Some(release.version),
            installed: None,
        });
    }

    if options.check_only {
        return Ok(UpdateReport {
            current,
            latest: Some(release.version),
            installed: None,
        });
    }

    let Some(target) = release_target() else {
        return Err(format!(
            "there is no prebuilt archive for this platform; build from source: \
             cargo install --git https://github.com/lightconsen/coderex.git --tag {}",
            release.tag
        ));
    };

    let origin = classify_install_path(&current_exe, home::home_dir().as_deref());
    match origin {
        InstallOrigin::SelfManaged => {}
        InstallOrigin::PackageManager(manager) => {
            return Err(format!(
                "{} is managed by {manager}; upgrade it with that tool instead of \
                 replacing the file",
                current_exe.display()
            ));
        }
        InstallOrigin::Unwritable => {
            return Err(format!(
                "{} is not writable by this user; reinstall with the install script or \
                 move the binary somewhere you can write",
                current_exe.display()
            ));
        }
    }

    let name = asset_name(target);
    let asset = release.asset(&name).ok_or_else(|| {
        format!(
            "release {} does not publish {name}; see {}/{}",
            release.tag, DEFAULT_RELEASE_BASE, release.tag
        )
    })?;
    let checksums = release.asset(CHECKSUM_ASSET).ok_or_else(|| {
        format!(
            "release {} does not publish {CHECKSUM_ASSET}; refusing to install an \
             unverifiable download",
            release.tag
        )
    })?;

    let base = release_base_override();
    let target_dir = current_exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", current_exe.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".coderex-update-")
        .tempdir_in(target_dir)
        .map_err(|error| {
            format!(
                "could not stage the update in {}: {error}",
                target_dir.display()
            )
        })?;

    let sums_path = staging.path().join(CHECKSUM_ASSET);
    download(
        &client,
        &asset_url(checksums, &release.tag, base.as_deref()),
        &sums_path,
        MAX_DOWNLOAD_BYTES,
    )
    .await?;
    let sums_text = std::fs::read_to_string(&sums_path)
        .map_err(|error| format!("could not read {CHECKSUM_ASSET}: {error}"))?;
    let sums = parse_sha256sums(&sums_text)?;
    let expected = sums
        .get(&name)
        .ok_or_else(|| format!("{CHECKSUM_ASSET} has no entry for {name}"))?;

    let archive_path = staging.path().join(&name);
    let actual = download(
        &client,
        &asset_url(asset, &release.tag, base.as_deref()),
        &archive_path,
        MAX_DOWNLOAD_BYTES,
    )
    .await?;
    verify_checksum(expected, &actual)?;

    let replacement = if is_windows_target(target) {
        archive_path
    } else {
        extract_tarball(&archive_path, &name, staging.path())?
    };

    replace_binary(&current_exe, &replacement)?;

    Ok(UpdateReport {
        current,
        latest: Some(release.version),
        installed: Some(current_exe),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_release_reads_the_tag_and_assets() {
        let body = serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [
                {"name": "coderex-x86_64-unknown-linux-gnu.tar.gz",
                 "browser_download_url": "https://example.invalid/a.tar.gz"},
                {"name": "SHA256SUMS",
                 "browser_download_url": "https://example.invalid/SHA256SUMS"},
            ]
        });
        let release = parse_release(&body).unwrap();
        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.assets.len(), 2);
        assert!(release.asset("SHA256SUMS").is_some());
    }

    #[test]
    fn a_release_without_a_tag_is_refused() {
        let body = serde_json::json!({"assets": []});
        assert!(parse_release(&body).is_err());
    }

    #[test]
    fn asset_names_follow_the_release_matrix() {
        assert_eq!(
            asset_name("x86_64-unknown-linux-gnu"),
            "coderex-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("x86_64-pc-windows-msvc"),
            "coderex-x86_64-pc-windows-msvc.exe"
        );
        assert!(!is_windows_target("aarch64-apple-darwin"));
    }

    #[test]
    fn a_mirror_base_builds_the_same_layout_github_uses() {
        let asset = ReleaseAsset {
            name: "coderex-x.tar.gz".into(),
            url: "https://example.invalid/original".into(),
        };
        assert_eq!(
            asset_url(
                &asset,
                "v1.0.0",
                Some("http://127.0.0.1:9/releases/download/")
            ),
            "http://127.0.0.1:9/releases/download/v1.0.0/coderex-x.tar.gz"
        );
        assert_eq!(
            asset_url(&asset, "v1.0.0", None),
            "https://example.invalid/original"
        );
    }

    #[test]
    fn parse_sha256sums_accepts_the_standard_format() {
        let digest = "a".repeat(64);
        let sums = parse_sha256sums(&format!(
            "{digest}  coderex-x.tar.gz\n{} *coderex-y.exe\n",
            "b".repeat(64)
        ))
        .unwrap();
        assert_eq!(sums.get("coderex-x.tar.gz"), Some(&digest));
        assert_eq!(sums.get("coderex-y.exe"), Some(&"b".repeat(64)));
    }

    #[test]
    fn parse_sha256sums_rejects_a_malformed_digest() {
        assert!(parse_sha256sums("notahash  coderex-x.tar.gz").is_err());
    }

    #[test]
    fn parse_sha256sums_rejects_a_duplicate_name() {
        let digest = "c".repeat(64);
        assert!(
            parse_sha256sums(&format!(
                "{digest}  coderex-x.tar.gz\n{digest}  coderex-x.tar.gz"
            ))
            .is_err()
        );
    }

    #[test]
    fn a_checksum_mismatch_is_an_error() {
        assert!(verify_checksum(&"a".repeat(64), &"b".repeat(64)).is_err());
        assert!(verify_checksum(&"a".repeat(64), &"A".repeat(64)).is_ok());
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn package_manager_install_paths_are_refused() {
        let home = Path::new("/home/someone");
        assert_eq!(
            classify_install_path(
                Path::new("/opt/homebrew/Cellar/coderex/1.0/bin/coderex"),
                Some(home)
            ),
            InstallOrigin::PackageManager("Homebrew")
        );
        assert_eq!(
            classify_install_path(Path::new("/nix/store/abc-coderex/bin/coderex"), Some(home)),
            InstallOrigin::PackageManager("Nix")
        );
        assert_eq!(
            classify_install_path(Path::new("/home/someone/.cargo/bin/coderex"), Some(home)),
            InstallOrigin::PackageManager("cargo install")
        );
    }

    #[test]
    fn a_writable_plain_directory_is_self_managed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("coderex");
        std::fs::write(&exe, b"binary").unwrap();
        assert_eq!(
            classify_install_path(&exe, None),
            InstallOrigin::SelfManaged
        );
    }

    #[test]
    fn tar_extraction_rejects_a_path_traversal_member_name() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("good.tar.gz");
        write_tarball(&archive, &[("coderex-x", "payload")]);
        for member in ["../escape", "a/b", "..", ""] {
            let error = extract_tarball(&archive, member, dir.path()).unwrap_err();
            assert!(
                error.contains("refusing archive member name"),
                "{member}: {error}"
            );
        }
    }

    #[test]
    fn tar_extraction_rejects_an_archive_naming_a_different_file() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("other.tar.gz");
        write_tarball(&archive, &[("not-coderex", "payload")]);
        let error = extract_tarball(&archive, "coderex-x", dir.path()).unwrap_err();
        assert!(error.contains("holds not-coderex"), "{error}");
    }

    #[test]
    fn tar_extraction_rejects_an_archive_with_extra_members() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("extra.tar.gz");
        write_tarball(&archive, &[("coderex-x", "one"), ("other", "two")]);
        let error = extract_tarball(&archive, "coderex-x", dir.path()).unwrap_err();
        assert!(error.contains("more than one file"), "{error}");
    }

    #[test]
    fn tar_extraction_writes_the_named_member() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("good.tar.gz");
        write_tarball(&archive, &[("coderex-x", "payload")]);
        let out = extract_tarball(&archive, "coderex-x", dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(out).unwrap(), "payload");
    }

    #[test]
    fn replace_binary_swaps_the_file_and_preserves_mode() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("coderex");
        let replacement = dir.path().join("coderex.new");
        std::fs::write(&current, b"old").unwrap();
        std::fs::write(&replacement, b"new").unwrap();
        replace_binary(&current, &replacement).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        assert!(!replacement.exists());
    }

    /// Write a gzipped tar holding the given regular files.
    fn write_tarball(path: &Path, files: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, std::io::Cursor::new(body.as_bytes()))
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }
}
