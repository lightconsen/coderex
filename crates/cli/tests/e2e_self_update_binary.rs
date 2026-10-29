//! `coderex update` against a fake release served over loopback.
//!
//! These run the real binary — a copy of it, so a successful update replaces
//! the copy and never the test build — against a tiny HTTP server that
//! publishes the same layout GitHub does: a `/releases/latest` document, the
//! archives, and `SHA256SUMS`.

use std::io::{BufRead, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use coderex_cli::self_update;

/// What the fake release publishes.
struct FakeRelease {
    tag: String,
    asset_name: String,
    asset: Vec<u8>,
    /// `None` makes the release omit `SHA256SUMS` entirely.
    checksums: Option<String>,
}

impl FakeRelease {
    fn new(tag: &str, asset_name: &str, asset: Vec<u8>) -> Self {
        let digest = self::sha256(&asset);
        Self {
            tag: tag.to_string(),
            asset_name: asset_name.to_string(),
            asset,
            checksums: Some(format!("{digest}  {asset_name}\n")),
        }
    }

    fn without_checksums(mut self) -> Self {
        self.checksums = None;
        self
    }

    fn with_checksum(mut self, digest: &str) -> Self {
        self.checksums = Some(format!("{digest}  {}\n", self.asset_name));
        self
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Serve `release` on loopback until the returned handle is dropped.
struct FakeServer {
    base: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FakeServer {
    fn start(release: FakeRelease) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("local address").port();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = std::sync::Arc::clone(&stop);

        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");

                let (status, body) = route(&release, path, port);
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });

        Self {
            base: format!("http://127.0.0.1:{port}"),
            stop,
        }
    }

    fn asset_base(&self) -> String {
        format!("{}/releases/download", self.base)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Unblock `incoming()` so the thread notices the flag.
        let _ = std::net::TcpStream::connect(self.base.trim_start_matches("http://"));
    }
}

fn route(release: &FakeRelease, path: &str, port: u16) -> (&'static str, Vec<u8>) {
    if path.ends_with("/releases/latest") {
        let asset_url = format!(
            "http://127.0.0.1:{port}/releases/download/{}/{}",
            release.tag, release.asset_name
        );
        let body = serde_json::json!({
            "tag_name": release.tag,
            "assets": [
                {"name": release.asset_name, "browser_download_url": asset_url},
                {"name": "SHA256SUMS",
                 "browser_download_url": format!("http://127.0.0.1:{port}/releases/download/{}/SHA256SUMS", release.tag)},
            ],
        });
        return ("200 OK", serde_json::to_vec(&body).expect("serialize"));
    }
    let suffix = format!("/{}/{}", release.tag, release.asset_name);
    if path.ends_with(&suffix) {
        return ("200 OK", release.asset.clone());
    }
    if path.ends_with(&format!("/{}/SHA256SUMS", release.tag)) {
        return match &release.checksums {
            Some(text) => ("200 OK", text.clone().into_bytes()),
            None => ("404 Not Found", b"missing".to_vec()),
        };
    }
    ("404 Not Found", b"missing".to_vec())
}

/// A copy of the built binary in a scratch directory, so an update replaces
/// the copy instead of the artifact under test.
fn copied_binary() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = PathBuf::from(env!("CARGO_BIN_EXE_coderex"));
    let name = if cfg!(windows) {
        "coderex.exe"
    } else {
        "coderex"
    };
    let copied = dir.path().join(name);
    std::fs::copy(&source, &copied).expect("copy the binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(0o755))
            .expect("make the copy executable");
    }
    (dir, copied)
}

/// The bytes a release ships for this platform.
fn payload() -> (String, Vec<u8>) {
    let target = self_update::release_target().expect("a supported platform");
    let name = self_update::asset_name(target);
    #[cfg(unix)]
    {
        let script = b"#!/bin/sh\necho fake-new-version\n";
        let archive = tarball(&name, script);
        (name, archive)
    }
    #[cfg(windows)]
    {
        let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_coderex")).expect("read the binary");
        bytes.extend_from_slice(b"\nFAKE-NEW-VERSION");
        (name, bytes)
    }
}

#[cfg(unix)]
fn tarball(member: &str, body: &[u8]) -> Vec<u8> {
    use std::io::Write as _;

    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_data(&mut header, member, std::io::Cursor::new(body))
        .expect("append the member");
    let tar = builder.into_inner().expect("finish the tar");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar).expect("gzip the tar");
    encoder.finish().expect("finish gzip")
}

fn run_update(binary: &Path, server: &FakeServer, args: &[&str]) -> std::process::Output {
    Command::new(binary)
        .arg("update")
        .args(args)
        .env("CODEREX_API_BASE_URL", &server.base)
        .env("CODEREX_RELEASE_BASE_URL", server.asset_base())
        .env("CODEREX_DISABLE_UPDATE_CHECK", "1")
        .env("CODEREX_LOG", "off")
        .output()
        .expect("run coderex update")
}

#[test]
fn update_check_reports_a_newer_release_without_replacing_the_binary() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let original = std::fs::read(&binary).expect("read the copy");

    let output = run_update(&binary, &server, &["--check"]);

    assert!(
        output.status.success(),
        "update --check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("99.0.0"), "{stdout}");
    assert_eq!(
        std::fs::read(&binary).expect("read the copy"),
        original,
        "--check replaced the binary"
    );
}

#[test]
fn update_downloads_verifies_and_replaces_the_binary() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset);
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();

    let output = run_update(&binary, &server, &[]);

    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Updated"), "{stdout}");

    #[cfg(unix)]
    {
        let replaced = std::fs::read(&binary).expect("read the replaced binary");
        assert_eq!(replaced, b"#!/bin/sh\necho fake-new-version\n");
        let run = Command::new(&binary).output().expect("run the new binary");
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            "fake-new-version"
        );
    }
    #[cfg(windows)]
    {
        let replaced = std::fs::read(&binary).expect("read the replaced binary");
        assert!(replaced.ends_with(b"\nFAKE-NEW-VERSION"));
    }
}

#[test]
fn a_bad_checksum_leaves_the_binary_untouched() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset).with_checksum(&"0".repeat(64));
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let original = std::fs::read(&binary).expect("read the copy");

    let output = run_update(&binary, &server, &[]);

    assert!(!output.status.success(), "a bad checksum was accepted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert_eq!(
        std::fs::read(&binary).expect("read the copy"),
        original,
        "a bad checksum replaced the binary"
    );
}

#[test]
fn a_release_without_sha256sums_is_refused() {
    let (tag_name, asset) = payload();
    let release = FakeRelease::new("v99.0.0", &tag_name, asset).without_checksums();
    let server = FakeServer::start(release);
    let (_dir, binary) = copied_binary();
    let original = std::fs::read(&binary).expect("read the copy");

    let output = run_update(&binary, &server, &[]);

    assert!(
        !output.status.success(),
        "an unverifiable release was installed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SHA256SUMS"), "{stderr}");
    assert_eq!(std::fs::read(&binary).expect("read the copy"), original);
}
