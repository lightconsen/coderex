//! Install-path test for archive signatures with the signing key supplied
//! through `CODEREX_PLUGIN_PUBLIC_KEY`.
//!
//! `std::env::set_var` is unsafe in Rust 2024 and this crate denies unsafe
//! code, so the environment cannot be changed inside the test process. The
//! parent test therefore runs the child test in a subprocess and sets the
//! variable on that process, which is the same path the CLI takes at startup.

use coderex_plugins::plugins::PluginManager;
use ed25519_dalek::Signer as _;

const CHILD_ENV: &str = "CODEREX_SIGNATURE_TEST_CHILD";
const SIGNING_KEY_BYTES: [u8; 32] = [7u8; 32];

fn archive_with(manifest_json: &str) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_json.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "plugin.json", manifest_json.as_bytes())
            .unwrap();
        builder.finish().unwrap();
    }
    encoder.finish().unwrap()
}

#[test]
fn a_correct_detached_signature_installs_with_the_environment_key() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }

    let key = ed25519_dalek::SigningKey::from_bytes(&SIGNING_KEY_BYTES);
    let archive = archive_with(r#"{"name":"env-signed","version":"1.0.0"}"#);
    let good_signature = hex::encode(key.sign(&archive).to_bytes());
    let bad_signature = hex::encode(key.sign(b"some other bytes").to_bytes());

    let dir = tempfile::tempdir().unwrap();
    let archive_path = dir.path().join("plugin.tar.gz");
    let good_path = dir.path().join("plugin.tar.gz.sig");
    let bad_path = dir.path().join("bad.sig");
    std::fs::write(&archive_path, &archive).unwrap();
    std::fs::write(&good_path, good_signature).unwrap();
    std::fs::write(&bad_path, bad_signature).unwrap();

    let public_key = hex::encode(key.verifying_key().to_bytes());

    // The correct detached signature installs.
    let status = run_child(
        &public_key,
        &archive_path,
        Some(&good_path),
        &dir.path().join("ok"),
    );
    assert!(
        status.status.success(),
        "a correct detached signature was refused; output:\n{}",
        status_text(&status)
    );
    assert!(
        dir.path()
            .join("ok")
            .join("env-signed")
            .join("plugin.json")
            .exists()
    );

    // A signature over different bytes is refused.
    let status = run_child(
        &public_key,
        &archive_path,
        Some(&bad_path),
        &dir.path().join("wrong"),
    );
    assert!(
        !status.status.success(),
        "a wrong detached signature was accepted"
    );
    assert!(!dir.path().join("wrong").join("env-signed").exists());

    // No signature at all is refused when the key is configured.
    let status = run_child(
        &public_key,
        &archive_path,
        None,
        &dir.path().join("missing"),
    );
    assert!(
        !status.status.success(),
        "a missing detached signature was accepted"
    );
    assert!(!dir.path().join("missing").join("env-signed").exists());
}

fn run_child(
    public_key: &str,
    archive: &std::path::Path,
    signature: Option<&std::path::Path>,
    install_dir: &std::path::Path,
) -> std::process::Output {
    let exe = std::env::current_exe().unwrap();
    let mut command = std::process::Command::new(exe);
    command
        .arg("--exact")
        .arg("child_installs_from_files")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env("CODEREX_PLUGIN_PUBLIC_KEY", public_key)
        .env("CODEREX_TEST_ARCHIVE", archive)
        .env("CODEREX_TEST_INSTALL", install_dir);
    if let Some(signature) = signature {
        command.env("CODEREX_TEST_SIGNATURE", signature);
    }
    command.output().unwrap()
}

fn status_text(output: &std::process::Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The child side of the environment test. It does nothing unless the parent
/// set [`CHILD_ENV`].
#[tokio::test]
async fn child_installs_from_files() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }

    let archive_path = std::env::var("CODEREX_TEST_ARCHIVE").expect("archive path");
    let install_dir = std::env::var("CODEREX_TEST_INSTALL").expect("install dir");
    let signature_path = std::env::var("CODEREX_TEST_SIGNATURE").ok();

    let archive = std::fs::read(archive_path).unwrap();
    let signature = signature_path.map(|path| std::fs::read_to_string(path).unwrap());

    let mut manager = PluginManager::new();
    assert!(
        manager.set_public_key_from_env(),
        "CODEREX_PLUGIN_PUBLIC_KEY was not picked up"
    );

    manager
        .install_archive_bytes(
            &archive,
            signature.as_deref().map(str::trim),
            std::path::Path::new(&install_dir),
        )
        .await
        .expect("the archive install should succeed in the child");
}
