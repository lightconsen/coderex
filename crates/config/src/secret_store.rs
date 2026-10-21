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
//!   created under the user's profile and its DACL is replaced with an
//!   explicit, protected DACL that grants only the current user's SID (read
//!   from the process token) full access, so inherited entries from the
//!   profile cannot widen it. The DACL is applied to the temporary file before
//!   it is renamed over the target, and the result is read back and verified.
//!   If that protected DACL cannot be applied and verified, the save fails
//!   closed: the temporary file is discarded and no token file is written,
//!   rather than persisting a credential under an inherited ACL. This path uses
//!   the native security APIs (`windows-sys`) and is compiled but has not been
//!   executed on a Windows host in this tree; see the module comments on
//!   [`tighten_to_owner_only`].
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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
                if let Err(error) = tighten_to_owner_only(&self.path) {
                    tracing::error!(
                        path = %self.path.display(),
                        "the leftover plaintext MCP token file could not be \
                         removed or tightened to a user-only DACL; it may remain \
                         readable by other principals: {error}"
                    );
                }
            }
        }
    }
}

#[cfg(feature = "keyring")]
// On macOS this is a unit variant; on Linux and Windows it constructs a
// backend that needs an allocation, so the shared signature is not `const`.
#[allow(clippy::missing_const_for_fn)]
fn default_backend() -> Backend {
    Backend::Secret(Arc::new(KeychainBackend))
}

#[cfg(not(feature = "keyring"))]
// On macOS this is a unit variant; on Linux and Windows it constructs a
// backend that needs an allocation, so the shared signature is not `const`.
#[allow(clippy::missing_const_for_fn)]
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
        // Fail closed: the fallback token file must never be renamed into place
        // with an inherited (possibly wider) DACL. If the protected user-only
        // DACL cannot be applied and verified, the `?` propagates and the temp
        // file is removed below, so the save fails instead of leaving a weakly
        // protected credential on disk.
        tighten_to_owner_only(&temp)?;
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
    let name = path.file_name().map_or_else(
        || "tokens".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".{name}.tmp-{}-{sequence}", std::process::id())
}

/// Make the file owner-readable/writable only.
///
/// On Unix the file is already created with mode `0600`, so this only repairs a
/// file that lost it; the error is reported to the caller, which decides
/// whether it matters. The Windows path, where a new file inherits the
/// directory's ACL, fails closed instead.
#[cfg(unix)]
fn tighten_to_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// The fixed part of a DACL that grants one SID full access.
///
/// The buffer is an `ACL` header followed by one `ACCESS_ALLOWED_ACE` whose
/// `SidStart` field is the first four bytes of the SID, so the bytes after the
/// ACL header are the ACE header, the access mask, and the SID. These constants
/// mirror the C layouts (`ACL` is 8 bytes, `ACE_HEADER` plus `Mask` is 8) and
/// are kept layout-free so the size arithmetic can be unit-tested on hosts
/// that are not Windows.
#[cfg(any(windows, test))]
const ACL_HEADER_BYTES: u32 = 8;
#[cfg(any(windows, test))]
const ACCESS_ALLOWED_ACE_HEADER_BYTES: u32 = 8;

/// How many bytes a DACL granting exactly `sid_len` bytes of SID needs.
#[cfg(any(windows, test))]
const fn user_only_dacl_size(sid_len: u32) -> u32 {
    ACL_HEADER_BYTES + ACCESS_ALLOWED_ACE_HEADER_BYTES + sid_len
}

/// Enforce a user-only DACL on the token file.
///
/// The native security APIs are used because `std` has no ACL interface and
/// `icacls` is both locale-sensitive and not atomic. The DACL is an explicit,
/// protected DACL containing one `ACCESS_ALLOWED_ACE` for the SID of the user
/// running the process. `PROTECTED_DACL_SECURITY_INFORMATION` detaches the
/// DACL from the parent directory's inheritable entries, so the profile's
/// inherited ACL cannot re-widen it. After setting it, the DACL is read back
/// and verified to contain exactly that one ACE.
///
/// This is applied to the temporary file before the rename, so the target
/// never exists with a wider ACL during a save.
///
/// Compile-gated and not executed by the test suite in this tree: it requires
/// a Windows host. Unlike the Unix path this fails closed: if the protected
/// user-only DACL cannot be applied and verified, the error is logged and
/// propagated so [`write_file`] refuses to rename the file into place rather
/// than leaving a credential with an inherited, possibly wider DACL.
#[cfg(windows)]
fn tighten_to_owner_only(path: &Path) -> io::Result<()> {
    match windows_acl::restrict_to_current_user(path) {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                "refusing to write the MCP token file without a protected \
                 user-only DACL: {error}"
            );
            Err(error)
        }
    }
}

/// Windows ACL construction over `windows-sys`.
///
/// The process token is queried for `TokenUser`; that SID is the only
/// principal the resulting DACL grants. The DACL buffer is built in a
/// `u64`-backed allocation so it is suitably aligned for the Win32 structures.
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 security calls; every call site has a SAFETY note.
mod windows_acl {
    use super::user_only_dacl_size;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, GENERIC_ALL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
        AddAccessAllowedAce, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetLengthSid, GetTokenInformation, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Build and install the DACL, then verify it.
    pub(super) fn restrict_to_current_user(path: &Path) -> io::Result<()> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let sid_buffer = current_user_sid()?;
        // SAFETY: `sid_buffer` was filled by `GetTokenInformation(TokenUser)`;
        // its first bytes are a `TOKEN_USER` whose `User.Sid` points inside the
        // same allocation.
        let sid = unsafe { sid_from_token_buffer(&sid_buffer) };
        if sid.is_null() {
            return Err(io::Error::other("the process token has no user SID"));
        }
        // SAFETY: `sid` points at a valid SID inside `sid_buffer`.
        let sid_len = unsafe { GetLengthSid(sid) };
        if sid_len == 0 {
            return Err(io::Error::last_os_error());
        }

        let dacl_bytes = user_only_dacl_size(sid_len);
        let mut dacl_buffer = vec![0_u64; (dacl_bytes as usize).div_ceil(8)];
        // SAFETY: the allocation is at least `dacl_bytes` long (rounded up to
        // a `u64` boundary) and sufficiently aligned for `ACL`.
        let dacl = dacl_buffer.as_mut_ptr().cast::<ACL>();
        if unsafe { InitializeAcl(dacl, dacl_bytes, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dacl` has room for the ACE (the size was computed from the
        // SID length), and `sid` is valid for the duration of the call.
        if unsafe { AddAccessAllowedAce(dacl, ACL_REVISION, GENERIC_ALL, sid) } == 0 {
            return Err(io::Error::last_os_error());
        }

        // A protected DACL: no inherited ACEs from the profile directory.
        // SAFETY: `wide` is NUL-terminated and `dacl` is a valid ACL in a live
        // buffer; the owner/group/SACL arguments are intentionally NULL.
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }

        verify(&wide, sid)
    }

    /// Query the process token for the current user's SID.
    ///
    /// Returned as the raw `TOKEN_USER` buffer, whose allocation must outlive
    /// any pointer to the SID inside it.
    fn current_user_sid() -> io::Result<Vec<u64>> {
        // SAFETY: pseudo-handle for the current process; the returned token
        // handle is closed below on every path.
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut needed = 0_u32;
            // The first call is expected to fail with ERROR_INSUFFICIENT_BUFFER
            // and report the required size.
            unsafe {
                GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            }
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
            // SAFETY: the buffer is at least `needed` bytes and aligned.
            let ok = unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(buffer)
        })();
        // SAFETY: `token` was opened successfully above and is not used after
        // this point.
        unsafe {
            CloseHandle(token);
        }
        result
    }

    /// The SID in a `GetTokenInformation(TokenUser)` buffer.
    ///
    /// # Safety
    ///
    /// `buffer` must contain a `TOKEN_USER` at offset zero and stay alive for
    /// as long as the returned pointer is used.
    unsafe fn sid_from_token_buffer(buffer: &[u64]) -> PSID {
        let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        token_user.User.Sid
    }

    /// Read the DACL back and confirm it grants only `expected_sid`.
    fn verify(wide: &[u16], expected_sid: PSID) -> io::Result<()> {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; the out-pointers are valid. A
        // successful call allocates `descriptor`, freed below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // SAFETY: `dacl` points into `descriptor` and is non-NULL on success.
        let verified = !dacl.is_null() && unsafe { dacl_grants_only(dacl, expected_sid) };
        // SAFETY: `GetNamedSecurityInfoW` documents that the descriptor is
        // freed with `LocalFree`.
        unsafe {
            LocalFree(descriptor);
        }
        if verified {
            Ok(())
        } else {
            Err(io::Error::other(
                "the token file DACL does not grant only the current user",
            ))
        }
    }

    /// Whether `dacl` has exactly one allow ACE, for `expected_sid`.
    ///
    /// # Safety
    ///
    /// `dacl` must be a valid ACL and `expected_sid` a valid SID.
    unsafe fn dacl_grants_only(dacl: *const ACL, expected_sid: PSID) -> bool {
        let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetAclInformation(
                dacl,
                std::ptr::addr_of_mut!(info).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if ok == 0 || info.AceCount != 1 {
            return false;
        }

        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
            return false;
        }
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        // `ACCESS_ALLOWED_ACE_TYPE` is 0; compare the byte-sized field.
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            return false;
        }
        // `SidStart` is the first DWORD of the SID copied into the ACE.
        let ace_sid: PSID = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
        unsafe { EqualSid(ace_sid, expected_sid) != 0 }
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

    /// Windows counterpart of the unix owner-only test.
    ///
    /// The save path installs a protected DACL granting only the current user,
    /// and `restrict_to_current_user` reads it back and fails if it grants
    /// anyone else. Gated on Windows so the Win32 calls actually execute on the
    /// Windows CI job — that is the only place they can run, and until this
    /// test existed nothing exercised them there.
    #[cfg(windows)]
    #[test]
    fn the_file_is_owner_only_after_a_save_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let persistence = TokenPersistence::file_only(&path);

        persistence.save(r#"{"states":["first"]}"#).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"states":["first"]}"#
        );
        // Re-tightening the file that is actually on disk exercises the
        // independent read-back verification.
        super::tighten_to_owner_only(&path).expect("the saved file has a user-only DACL");

        persistence.save(r#"{"states":["second"]}"#).unwrap();
        super::tighten_to_owner_only(&path).expect("the overwritten file has a user-only DACL");
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
        let persistence = TokenPersistence::with_backend(
            &path,
            Arc::new(BrokenBackend) as Arc<dyn SecretBackend>,
        );

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

    /// The Windows DACL buffer must hold the ACL header, the allow-ACE header
    /// and mask, and the whole SID. Real SIDs are multiples of four bytes, so
    /// the size stays DWORD-aligned as `InitializeAcl` expects.
    #[test]
    fn a_user_only_dacl_is_sized_for_the_sid_and_word_aligned() {
        for sid_len in [8_u32, 12, 16, 28, 32] {
            let size = user_only_dacl_size(sid_len);
            assert_eq!(size, 16 + sid_len);
            assert_eq!(size % 4, 0, "size {size} is not DWORD-aligned");
            assert!(
                size > sid_len + 8,
                "size {size} cannot also hold the ACL and ACE headers"
            );
        }
    }

    /// The `u64`-backed allocation used for the DACL rounds up without losing
    /// any of the bytes `InitializeAcl` is told about.
    #[test]
    fn the_dacl_allocation_rounds_up_to_u64() {
        for sid_len in [4_u32, 8, 12, 16, 28, 32] {
            let bytes = user_only_dacl_size(sid_len) as usize;
            let words = bytes.div_ceil(std::mem::size_of::<u64>());
            assert!(words * std::mem::size_of::<u64>() >= bytes);
        }
    }
}
