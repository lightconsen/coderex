//! Private persistence for small credential documents.
//!
//! Two crates persist the same MCP OAuth document — `coderex-mcp`'s
//! `OAuthManager` and `coderex-tools`' `McpAuth` tool — and neither can depend
//! on the other. This module is the shared persistence layer both use, so the
//! file shape, file mode and keychain choice cannot drift apart.
//!
//! # Where the bytes live
//!
//! When the `keyring` feature is enabled, the document is stored in the OS
//! keychain (macOS Keychain, Windows Credential Manager, or the freedesktop
//! Secret Service on Linux). If the keychain is unavailable or an operation
//! fails, the document is written to the file instead:
//!
//! * On Unix the file is created and replaced with mode `0600`. Writes go to a
//!   sibling temporary file that is created with mode `0600` and then renamed
//!   over the target, so an existing (possibly world-readable) file is never
//!   rewritten in place and there is no window where the contents are exposed.
//! * On Windows the keychain is the primary protection. The file fallback is
//!   created under the user's profile, whose inherited ACL is normally
//!   user-only, and a best-effort `icacls` tightening is attempted; without a
//!   native ACL API in `std` this cannot be guaranteed, so a Windows host
//!   without a working keychain is a weaker configuration.
//!
//! A plaintext file left behind by an older version is migrated on the next
//! successful keychain read or write: it is removed, or tightened to `0600` if
//! removal fails.
//!
//! Set `CODEREX_DISABLE_KEYCHAIN` (to anything) to force the private-file path.
//! That is useful in CI, where an interactive keychain may not be available.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Service name used for keychain entries.
pub const KEYCHAIN_SERVICE: &str = "coderex";

/// An error from an injectable secret backend.
#[derive(Debug, thiserror::Error)]
pub enum SecretStoreError {
    /// The platform keychain could not be used.
    #[error("keychain operation failed: {0}")]
    Keychain(String),
}

/// A place secrets can be stored, separate from the fallback file.
///
/// Implementations exist for the real platform keychain ([`KeychainBackend`],
/// with the `keyring` feature) and for tests, which inject an in-memory backend
/// so they never touch a developer's real keychain.
pub trait SecretBackend: Send + Sync + std::fmt::Debug {
    /// Read the secret stored under `account`, if there is one.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot be reached. A missing entry is
    /// `Ok(None)`, not an error.
    fn get(&self, account: &str) -> Result<Option<String>, SecretStoreError>;

    /// Store `secret` under `account`, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot be reached or refuses the
    /// write.
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretStoreError>;

    /// Remove the secret stored under `account`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot be reached.
    fn delete(&self, account: &str) -> Result<(), SecretStoreError>;
}

/// The platform keychain, backed by `keyring-rs`.
///
/// On a platform where the `keyring` crate was built without a native backend
/// it uses an in-memory mock; this type is only compiled with the `keyring`
/// feature, whose feature definition enables the native backends.
#[cfg(feature = "keyring")]
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainBackend;

#[cfg(feature = "keyring")]
impl SecretBackend for KeychainBackend {
    fn get(&self, account: &str) -> Result<Option<String>, SecretStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account)
            .map_err(|error| SecretStoreError::Keychain(error.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(SecretStoreError::Keychain(error.to_string())),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account)
            .map_err(|error| SecretStoreError::Keychain(error.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|error| SecretStoreError::Keychain(error.to_string()))
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account)
            .map_err(|error| SecretStoreError::Keychain(error.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(SecretStoreError::Keychain(error.to_string())),
        }
    }
}

#[derive(Clone)]
enum Backend {
    /// The private file is the only destination.
    FileOnly,
    /// Try the secret backend first, then fall back to the private file.
    Secret(Arc<dyn SecretBackend>),
}

/// The private home of one credential document.
///
/// Cloning shares the warning state, so the "keychain unavailable" warning is
/// emitted once per document per process rather than once per request.
#[derive(Clone)]
pub struct TokenPersistence {
    path: PathBuf,
    account: String,
    backend: Backend,
    warned: Arc<AtomicBool>,
    fallback_warnings: Arc<AtomicUsize>,
}

impl std::fmt::Debug for TokenPersistence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenPersistence")
            .field("path", &self.path)
            .field("account", &self.account)
            .field(
                "backend",
                &match self.backend {
                    Backend::FileOnly => "file-only",
                    Backend::Secret(_) => "secret-backend",
                },
            )
            .finish_non_exhaustive()
    }
}

impl TokenPersistence {
    /// Persist the `mcp_tokens` document at `path`, preferring the OS keychain.
    ///
    /// When the `keyring` feature is disabled, or `CODEREX_DISABLE_KEYCHAIN` is
    /// set, this behaves like [`Self::file_only`].
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::new_for(path, "mcp_tokens")
    }

    /// Like [`Self::new`], with an explicit keychain account name.
    #[must_use]
    pub fn new_for(path: impl Into<PathBuf>, account: impl Into<String>) -> Self {
        let path = path.into();
        let account = account.into();
        let backend = if std::env::var_os("CODEREX_DISABLE_KEYCHAIN").is_some() {
            Backend::FileOnly
        } else {
            default_backend()
        };
        Self::with_backend_choice(path, account, backend)
    }

    /// Persist the document in a `0600` file, never consulting a keychain.
    ///
    /// This is the constructor for an explicit path — tests and callers that
    /// named a file asked for that file — and for hosts deliberately avoiding
    /// an interactive keychain.
    #[must_use]
    pub fn file_only(path: impl Into<PathBuf>) -> Self {
        Self::file_only_for(path, "mcp_tokens")
    }

    /// Like [`Self::file_only`], with an explicit keychain account name.
    #[must_use]
    pub fn file_only_for(path: impl Into<PathBuf>, account: impl Into<String>) -> Self {
        Self::with_backend_choice(path.into(), account.into(), Backend::FileOnly)
    }

    /// Persist the document through an injected backend, falling back to the
    /// `0600` file when the backend fails.
    ///
    /// Tests use this to exercise the keychain path without touching the real
    /// developer keychain.
    #[must_use]
    pub fn with_backend(path: impl Into<PathBuf>, backend: Arc<dyn SecretBackend>) -> Self {
        Self::with_backend_for(path, "mcp_tokens", backend)
    }

    /// Like [`Self::with_backend`], with an explicit keychain account name.
    #[must_use]
    pub fn with_backend_for(
        path: impl Into<PathBuf>,
        account: impl Into<String>,
        backend: Arc<dyn SecretBackend>,
    ) -> Self {
        Self::with_backend_choice(path.into(), account.into(), Backend::Secret(backend))
    }

    fn with_backend_choice(path: PathBuf, account: String, backend: Backend) -> Self {
        Self {
            path,
            account,
            backend,
            warned: Arc::new(AtomicBool::new(false)),
            fallback_warnings: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The file this persistence falls back to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The keychain account name.
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// How many times the one-time fallback warning has been emitted.
    ///
    /// Exposed for tests: it must stay at one no matter how many operations
    /// fail over the life of this value.
    #[must_use]
    pub fn fallback_warning_count(&self) -> usize {
        self.fallback_warnings.load(Ordering::SeqCst)
    }

    /// Read the document.
    ///
    /// The keychain is consulted first. A missing keychain entry falls through
    /// to the file (this is what migrates a file written by an older version);
    /// a keychain error falls through with a one-time warning.
    ///
    /// # Errors
    ///
    /// Returns an error only when the file exists but cannot be read.
    pub fn load(&self) -> io::Result<Option<String>> {
        match &self.backend {
            Backend::FileOnly => read_file(&self.path),
            Backend::Secret(backend) => match backend.get(&self.account) {
                Ok(Some(secret)) => {
                    // The plaintext copy is stale; do not leave a secret in
                    // two places.
                    self.remove_or_tighten_file();
                    Ok(Some(secret))
                }
                Ok(None) => read_file(&self.path),
                Err(error) => {
                    self.warn_fallback(&error.to_string());
                    read_file(&self.path)
                }
            },
        }
    }

    /// Write the document.
    ///
    /// The keychain is tried first. If it refuses, the document is written to
    /// the private file instead and the failure is warned about once.
    ///
    /// # Errors
    ///
    /// Returns an error when neither destination could be written (or when the
    /// file is the chosen destination and cannot be written).
    pub fn save(&self, content: &str) -> io::Result<()> {
        match &self.backend {
            Backend::FileOnly => write_file(&self.path, content),
            Backend::Secret(backend) => match backend.set(&self.account, content) {
                Ok(()) => {
                    self.remove_or_tighten_file();
                    Ok(())
                }
                Err(error) => {
                    self.warn_fallback(&error.to_string());
                    write_file(&self.path, content)
                }
            },
        }
    }

    /// Delete the document from both destinations.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists and cannot be removed. A backend
    /// that cannot be reached is logged rather than returned: the file is the
    /// last-resort copy and its removal is the part that matters here.
    pub fn delete(&self) -> io::Result<()> {
        if let Backend::Secret(backend) = &self.backend {
            if let Err(error) = backend.delete(&self.account) {
                tracing::debug!("keychain delete failed: {error}");
            }
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn warn_fallback(&self, error: &str) {
        if !self.warned.swap(true, Ordering::SeqCst) {
            self.fallback_warnings.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(
                path = %self.path.display(),
                "the OS keychain is unavailable or refused the MCP credential; \
                 falling back to a private 0600 file: {error}"
            );
        }
    }

    /// A stale plaintext file must not survive once the keychain owns the
    /// secret. Removal is preferred; if it fails, tighten it.
    fn remove_or_tighten_file(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "could not remove the plaintext MCP token file after storing \
                     the credential in the keychain; tightening it instead: {error}"
                );
                tighten_to_owner_only(&self.path);
            }
        }
    }
}

#[cfg(feature = "keyring")]
fn default_backend() -> Backend {
    Backend::Secret(Arc::new(KeychainBackend))
}

#[cfg(not(feature = "keyring"))]
fn default_backend() -> Backend {
    Backend::FileOnly
}

fn read_file(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Write `content` to `path` atomically with owner-only permissions.
fn write_file(path: &Path, content: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let temp = parent.join(temp_name(path));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // The mode is set at creation: there is no moment at which the file
        // exists with the process umask's wider permissions.
        options.mode(0o600);
    }

    let result = (|| -> io::Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        #[cfg(windows)]
        tighten_to_owner_only(&temp);
        // Renaming replaces the old file wholesale, so an existing
        // world-readable file never receives the new contents in place.
        std::fs::rename(&temp, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn temp_name(path: &Path) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map_or_else(|| "tokens".to_string(), |name| name.to_string_lossy().into_owned());
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".{name}.tmp-{}-{sequence}", std::process::id())
}

/// Make the file owner-readable/writable only.
#[cfg(unix)]
fn tighten_to_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            path = %path.display(),
            "could not set owner-only permissions on the MCP token file: {error}"
        );
    }
}

/// Best-effort owner-only ACL on Windows.
///
/// `std` has no ACL API, so the inherited user-profile ACL is the real
/// guarantee here. `icacls` is used to remove inheritance and grant only the
/// current user when it is available; failure is logged and does not fail the
/// write, because the keychain is the preferred location and the document must
/// still be persisted.
#[cfg(windows)]
fn tighten_to_owner_only(path: &Path) {
    let Some(user) = std::env::var("USERNAME").ok().filter(|user| !user.is_empty()) else {
        return;
    };
    let grant = format!("{user}:F");
    let outcome = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(&grant)
        .output();
    match outcome {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            path = %path.display(),
            status = %output.status,
            "could not restrict the MCP token file ACL to the current user"
        ),
        Err(error) => tracing::warn!(
            path = %path.display(),
            "could not run icacls to restrict the MCP token file ACL: {error}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory backend, so tests never touch the developer keychain.
    #[derive(Debug, Default)]
    struct MemoryBackend {
        values: Mutex<HashMap<String, String>>,
    }

    impl MemoryBackend {
        fn value(&self, account: &str) -> Option<String> {
            self.values.lock().unwrap().get(account).cloned()
        }
    }

    impl SecretBackend for MemoryBackend {
        fn get(&self, account: &str) -> Result<Option<String>, SecretStoreError> {
            Ok(self.value(account))
        }

        fn set(&self, account: &str, secret: &str) -> Result<(), SecretStoreError> {
            self.values
                .lock()
                .unwrap()
                .insert(account.to_string(), secret.to_string());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
            self.values.lock().unwrap().remove(account);
            Ok(())
        }
    }

    /// A backend that refuses every operation.
    #[derive(Debug, Default)]
    struct BrokenBackend;

    impl SecretBackend for BrokenBackend {
        fn get(&self, _account: &str) -> Result<Option<String>, SecretStoreError> {
            Err(SecretStoreError::Keychain("unavailable".into()))
        }

        fn set(&self, _account: &str, _secret: &str) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::Keychain("unavailable".into()))
        }

        fn delete(&self, _account: &str) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::Keychain("unavailable".into()))
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only_after_a_save_and_after_an_overwrite() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let persistence = TokenPersistence::file_only(&path);

        persistence.save(r#"{"states":["first"]}"#).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"states":["first"]}"#
        );

        // A pre-existing wider file must not leave a readable window on the
        // next write: the replacement is created 0600 and renamed over it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        persistence.save(r#"{"states":["second"]}"#).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"states":["second"]}"#
        );

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != std::ffi::OsStr::new("mcp_tokens.json"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files were left behind");
    }

    #[test]
    fn an_injected_backend_stores_and_retrieves_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let backend = Arc::new(MemoryBackend::default());
        let persistence =
            TokenPersistence::with_backend(&path, Arc::clone(&backend) as Arc<dyn SecretBackend>);

        persistence.save("secret-document").unwrap();
        assert_eq!(
            backend.value("mcp_tokens").as_deref(),
            Some("secret-document")
        );
        assert!(
            !path.exists(),
            "the plaintext file was left behind after a keychain save"
        );
        assert_eq!(
            persistence.load().unwrap().as_deref(),
            Some("secret-document")
        );

        persistence.delete().unwrap();
        assert!(backend.value("mcp_tokens").is_none());
        assert!(persistence.load().unwrap().is_none());
    }

    #[test]
    fn a_secret_loaded_from_the_keychain_removes_a_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        std::fs::write(&path, "legacy-plaintext").unwrap();

        let backend = Arc::new(MemoryBackend::default());
        backend
            .set("mcp_tokens", "from-keychain")
            .expect("in-memory set");
        let persistence =
            TokenPersistence::with_backend(&path, Arc::clone(&backend) as Arc<dyn SecretBackend>);

        assert_eq!(
            persistence.load().unwrap().as_deref(),
            Some("from-keychain")
        );
        assert!(!path.exists(), "the legacy plaintext file survived");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_backend_falls_back_to_a_private_file_and_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let persistence =
            TokenPersistence::with_backend(&path, Arc::new(BrokenBackend) as Arc<dyn SecretBackend>);

        persistence.save("first").unwrap();
        assert_eq!(persistence.fallback_warning_count(), 1);
        assert_eq!(mode(&path), 0o600);

        // A second failed operation must not warn again.
        persistence.save("second").unwrap();
        assert_eq!(persistence.fallback_warning_count(), 1);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        // Reading through the broken backend falls back to the file too.
        assert_eq!(persistence.load().unwrap().as_deref(), Some("second"));
        assert_eq!(persistence.fallback_warning_count(), 1);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = TokenPersistence::file_only(dir.path().join("absent.json"));
        assert!(persistence.load().unwrap().is_none());
    }
}
