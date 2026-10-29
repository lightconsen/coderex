//! Windows filesystem and network isolation for sandboxed subprocesses.
//!
//! The Windows backend used to be a Job Object and nothing else: it bounded
//! CPU, memory and UI reach, but a child could still read and write anything
//! its user could and use the network. This module adds a real security
//! boundary on top:
//!
//! 1. A per-user **AppContainer profile** is created (or an existing one is
//!    derived) and its SID is the identity the child runs under. An
//!    AppContainer token contains the container SID instead of the user's SID,
//!    so ordinary user files are unreadable unless the container SID was
//!    granted access.
//! 2. The container SID is granted read/execute on the configured read and
//!    execute paths and read/write on the configured write paths with
//!    `SetEntriesInAclW` + `SetNamedSecurityInfoW`. System roots are granted
//!    best-effort; they normally already include `ALL APPLICATION PACKAGES`.
//! 3. No capabilities are placed in the token unless `allow_network` is set.
//!    Without the `internetClient` capability an AppContainer has no network
//!    access, which is how this backend denies the network.
//!
//! ## Two ways to create the process
//!
//! The documented Windows way to start a process in an AppContainer is a
//! `STARTUPINFOEX` carrying `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` and
//! `CreateProcessW(..., EXTENDED_STARTUPINFO_PRESENT, ...)`. It is implemented
//! by `AppContainer::spawn`, which is **test-only**: the stable-Rust
//! `std::process::Command` cannot pass a process attribute list, so no
//! production caller can use it. The Windows test calls it to exercise the
//! documented primitive.
//!
//! Production instead creates the child with `CREATE_SUSPENDED`, then
//! `AppContainer::restrict_suspended` replaces its primary token with an
//! AppContainer token before the first thread runs, and `AppContainer::resume`
//! lets it go. The token is created with the undocumented-but-long-standing
//! `CreateAppContainerToken` export from `kernelbase.dll`; the call is resolved
//! dynamically and the sandbox reports itself unavailable when it is missing.
//! This is the same mechanism Chromium used to attach a LowBox token to a
//! suspended process.
//!
//! Both paths share the profile, SID, capability and ACL policy in this module.
//!
//! ## What is pure and what is Windows-only
//!
//! `acl_plan`, `capability_names`, `quote_windows_arg` and
//! `build_command_line` are deliberately platform-independent: they encode
//! the policy and are unit-tested on macOS and Linux as well. Every Win32 call
//! is behind `cfg(windows)`.
//!
//! ## Verification status
//!
//! The Windows-gated tests that spawn a real process are run by the
//! `test-windows` CI job. That job is currently advisory while it builds a
//! stable history, so treat the AppContainer path as reviewed-only until it
//! passes there consistently.

#![allow(unsafe_code)]
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use crate::sandbox::SandboxConfig;
use std::path::PathBuf;

/// The capability a container needs in order to reach the network.
///
/// It is deliberately absent by default: an AppContainer without it cannot
/// open outbound connections.
pub(crate) const INTERNET_CLIENT_CAPABILITY: &str = "internetClient";

/// What an ACL grant allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AclAccess {
    /// Traverse a directory in the path to something else.
    ///
    /// An AppContainer token does not carry `SeChangeNotifyPrivilege`, so it
    /// cannot bypass traverse checking: every ancestor of an allowed path has
    /// to grant `FILE_TRAVERSE` explicitly. This is not read or list access.
    Traverse,
    /// Read and execute (for a directory: list and traverse). No writes.
    ReadExecute,
    /// Read, execute and write.
    Modify,
}

impl AclAccess {
    /// Strength order used when two policy sources name the same path.
    const fn rank(self) -> u8 {
        match self {
            Self::Traverse => 0,
            Self::ReadExecute => 1,
            Self::Modify => 2,
        }
    }

    /// Whether this grant may be inherited by descendants.
    ///
    /// Traverse is deliberately not inheritable: it exists only to reach the
    /// granted path, not to widen access to everything below an ancestor.
    const fn inheritable(self) -> bool {
        !matches!(self, Self::Traverse)
    }
}

/// One path the container SID is granted access to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AclGrant {
    /// The path to change the DACL of.
    pub(crate) path: PathBuf,
    /// The access to grant.
    pub(crate) access: AclAccess,
    /// Whether the grant must also be applied to existing descendants.
    ///
    /// Inheritable ACEs cover files created *after* the grant. A workspace
    /// already full of files needs the walk or the container cannot read them.
    pub(crate) recursive: bool,
    /// Whether failure to change this path's DACL is survivable.
    ///
    /// System and `PATH` roots are best-effort: Windows system directories
    /// normally grant `ALL APPLICATION PACKAGES`, and a user may not be able
    /// to rewrite their DACL at all. The configured policy paths are required.
    pub(crate) best_effort: bool,
}

/// Build the ACL grants that confine a container.
///
/// Pure, so the policy can be read and tested on any host. The Win32 layer
/// supplies the platform's system roots and `PATH` directories; the test
/// supplies fixed ones.
///
/// Ordering is fixed (system roots, `PATH`, execute, read, write, working
/// directories) and duplicates are merged so the same path receives at most
/// one ACE and the strongest access wins.
///
/// There is deliberately **no blanket grant of the user's temp tree**: that
/// would hand every sandbox a shared, world-writable-by-low-integrity
/// directory. The Windows sandbox creates a private temp directory and names
/// it in `allowed_write_paths` instead.
#[must_use]
pub(crate) fn acl_plan(
    config: &SandboxConfig,
    system_roots: &[PathBuf],
    search_paths: &[PathBuf],
) -> Vec<AclGrant> {
    let mut grants: Vec<AclGrant> = Vec::new();

    for path in system_roots {
        push_path(&mut grants, path, AclAccess::ReadExecute, false, true);
    }
    for path in search_paths {
        push_path(&mut grants, path, AclAccess::ReadExecute, false, true);
    }
    for path in &config.allowed_exec_paths {
        push_path(&mut grants, path, AclAccess::ReadExecute, true, false);
    }
    for path in &config.allowed_read_paths {
        push_path(&mut grants, path, AclAccess::ReadExecute, true, false);
    }
    for path in &config.allowed_write_paths {
        push_path(&mut grants, path, AclAccess::Modify, true, false);
    }
    for path in &config.additional_working_dirs {
        push_path(&mut grants, path, AclAccess::Modify, true, false);
    }

    grants
}

/// Add a path and the traverse rights its ancestors need.
fn push_path(
    grants: &mut Vec<AclGrant>,
    path: &std::path::Path,
    access: AclAccess,
    recursive: bool,
    best_effort: bool,
) {
    push_grant(grants, path.to_path_buf(), access, recursive, best_effort);
    // Best-effort roots (the system directories and `PATH` entries) already
    // assume the platform grants the container access — that is what
    // "best-effort" means here, and it is true because those directories grant
    // ALL APPLICATION PACKAGES. Their ancestors are no different, and granting
    // each one costs a `SetNamedSecurityInfoW`; on the Windows CI runner `C:\`
    // alone took 252 seconds, plus 46s for `C:\hostedtoolcache` and 24s for
    // Chocolatey. Policy paths (the workspace and the private temp directory)
    // are the ones the container cannot otherwise reach, so only those get
    // their ancestor chain.
    if best_effort {
        return;
    }
    for ancestor in path.ancestors().skip(1) {
        // A relative path's last ancestor is the empty path; there is nothing
        // to grant there.
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        // A drive root (`C:\`) grants ALL APPLICATION PACKAGES read and execute
        // on every supported Windows installation, and rewriting its DACL is
        // pathologically slow: one write took 194.7 seconds on the CI runner,
        // the single largest cost in creating a sandbox.
        if is_root_like(ancestor) {
            continue;
        }
        // Ancestors get traverse rights only, and are always best-effort: a
        // user cannot rewrite the DACL of `C:\` or another user's directory,
        // and failing the whole sandbox because of that would be wrong.
        push_grant(
            grants,
            ancestor.to_path_buf(),
            AclAccess::Traverse,
            false,
            true,
        );
    }
}

/// Strip a verbatim (`\\?\`) prefix from a path.
///
/// `std::fs::canonicalize` returns verbatim paths on Windows, and a DACL call
/// on `\\?\C:\` is not the same object as one on `C:\`: the grants would be
/// duplicated (one of them ineffective) and the verbatim root would be granted
/// in a way that can never succeed.
fn normalize_path(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// Merge a grant into the plan, keeping one entry per path.
fn push_grant(
    grants: &mut Vec<AclGrant>,
    path: PathBuf,
    access: AclAccess,
    recursive: bool,
    best_effort: bool,
) {
    let path = normalize_path(path);
    if let Some(existing) = grants.iter_mut().find(|grant| grant.path == path) {
        if access.rank() > existing.access.rank() {
            existing.access = access;
        }
        existing.recursive |= recursive && access.inheritable();
        // If any source requires the path, the merge is required.
        existing.best_effort &= best_effort;
        return;
    }
    grants.push(AclGrant {
        path,
        access,
        recursive,
        best_effort,
    });
}

/// The capabilities an AppContainer token is given.
///
/// Pure: the only capability this backend ever grants is `internetClient`, and
/// only when the policy allows the network. Anything else would silently widen
/// the boundary.
#[must_use]
pub(crate) fn capability_names(config: &SandboxConfig) -> Vec<&'static str> {
    if config.allow_network {
        vec![INTERNET_CLIENT_CAPABILITY]
    } else {
        Vec::new()
    }
}

/// Quote one argument for the Windows command line.
///
/// `CreateProcessW` parses its command line itself; an argument containing a
/// space, tab or quote has to be wrapped in quotes and its backslashes/quotes
/// escaped the way `CommandLineToArgvW` expects.
#[must_use]
pub(crate) fn quote_windows_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty() || arg.contains([' ', '\t', '"']);
    if !needs_quotes {
        return arg.to_string();
    }

    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, then the quote is
                // escaped.
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    // Backslashes immediately before the closing quote are doubled.
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// Build the `lpCommandLine` for [`CreateProcessW`] from a program and
/// arguments.
#[must_use]
pub(crate) fn build_command_line(program: &str, args: &[String]) -> String {
    let mut line = quote_windows_arg(program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_windows_arg(arg));
    }
    line
}

/// Whether `path` is a filesystem root (`/`, `C:\`, or the `C:/` spelling the
/// cross-platform tests use).
///
/// `file_name()` is enough on Windows (a drive root has none) but not on a Unix
/// host, where `C:/` is an ordinary one-component relative path.
#[must_use]
fn is_root_like(path: &std::path::Path) -> bool {
    path.file_name().is_none() || path.components().count() <= 1
}

/// Whether an ACE mask grants every requested bit.
///
/// ACEs commonly store the generic bits (`GENERIC_READ`, `GENERIC_EXECUTE`,
/// `GENERIC_ALL`) rather than the specific rights they stand for. Comparing the
/// raw mask against `FILE_TRAVERSE | FILE_READ_ATTRIBUTES` therefore said "not
/// granted" for directories that already grant the container access, and every
/// sandbox creation rewrote them. `C:\` alone cost 238 seconds of
/// `SetNamedSecurityInfoW` on the Windows CI runner.
///
/// Pure, so the expansion is tested on every platform.
#[must_use]
pub(crate) const fn mask_covers(mask: u32, needed: u32) -> bool {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_EXECUTE: u32 = 0x2000_0000;
    const GENERIC_ALL: u32 = 0x1000_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_READ_DATA: u32 = 0x0000_0001;
    const FILE_WRITE_DATA: u32 = 0x0000_0002;
    const FILE_APPEND_DATA: u32 = 0x0000_0004;
    const FILE_READ_EA: u32 = 0x0000_0008;
    const FILE_WRITE_EA: u32 = 0x0000_0010;
    const FILE_EXECUTE: u32 = 0x0000_0020;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;

    if mask & needed == needed || mask & GENERIC_ALL != 0 {
        return true;
    }
    let mut effective = mask;
    if mask & GENERIC_READ != 0 {
        effective |=
            FILE_READ_DATA | FILE_READ_EA | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE;
    }
    if mask & GENERIC_WRITE != 0 {
        effective |= FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | FILE_WRITE_EA
            | FILE_WRITE_ATTRIBUTES
            | READ_CONTROL
            | SYNCHRONIZE;
    }
    if mask & GENERIC_EXECUTE != 0 {
        // `FILE_EXECUTE` is the directory form of `FILE_TRAVERSE`.
        effective |= FILE_EXECUTE | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE;
    }
    effective & needed == needed
}

// ─── Win32 implementation ───────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod imp {
    use super::{AclAccess, acl_plan, capability_names, mask_covers};
    use crate::sandbox::{SandboxConfig, SandboxError};
    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        CreateWellKnownSid, EqualSid, FreeSid, GetAce, GetAclInformation, GetSidSubAuthority,
        GetSidSubAuthorityCount, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES,
        SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, SYSTEM_MANDATORY_LABEL_ACE,
        WinBuiltinAnyPackageSid, WinCapabilityInternetClientSid,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, SetFileSecurityW,
    };
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    };
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleHandleW, GetProcAddress, LoadLibraryW,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTUPINFOEXW,
        TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    /// `SE_GROUP_ENABLED`; windows-sys keeps it behind another feature.
    const SE_GROUP_ENABLED: u32 = 0x4;
    /// `ProcessAccessToken` information class for `NtSetInformationProcess`.
    const PROCESS_ACCESS_TOKEN_CLASS: u32 = 9;
    /// `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE`.
    const INHERIT_ALL: u32 = 0x1 | 0x2;
    /// Stop walking a large tree after this many entries rather than spending
    /// unbounded time rewriting DACLs.
    const MAX_ACL_WALK_ENTRIES: usize = 200_000;

    // NtDll entry points that windows-sys does not bind.
    //
    // SAFETY: these are ordinary Win32/NT ABI declarations; the calls below
    // pass valid process handles and correctly sized structures.
    unsafe extern "system" {
        fn NtSetInformationProcess(
            ProcessHandle: HANDLE,
            ProcessInformationClass: u32,
            ProcessInformation: *const c_void,
            ProcessInformationLength: u32,
        ) -> i32;
        fn NtResumeProcess(ProcessHandle: HANDLE) -> i32;
    }

    /// The structure `NtSetInformationProcess(ProcessAccessToken)` expects.
    #[repr(C)]
    struct ProcessAccessToken {
        token: HANDLE,
        thread: HANDLE,
    }

    /// `CreateAppContainerToken` as exported by `kernelbase.dll`.
    ///
    /// The signature is not in the public SDK but has been stable since Windows
    /// 8 and is the one Chromium uses.
    type CreateAppContainerTokenFn = unsafe extern "system" fn(
        base_token: HANDLE,
        security_capabilities: *const SECURITY_CAPABILITIES,
        token: *mut HANDLE,
    ) -> i32;

    /// The function-pointer shape `GetProcAddress` returns.
    type RawProc = unsafe extern "system" fn() -> isize;

    /// Track whether the AppContainer runtime is present, once per process.
    static AVAILABLE: OnceLock<bool> = OnceLock::new();

    /// Roots whose DACLs have already been updated by this process, keyed by
    /// profile name, path, access and whether the tree was walked.
    ///
    /// The profile SIDs are stable, so re-granting on every command would only
    /// rewrite identical ACEs. New files inherit the directory ACE, so the
    /// walk is needed once per root per profile per process. The profile name is
    /// part of the key because the network and no-network profiles have
    /// different SIDs.
    type GrantedRoot = (&'static str, PathBuf, AclAccess, bool);
    static GRANTED_ROOTS: OnceLock<Mutex<Vec<GrantedRoot>>> = OnceLock::new();

    /// Whether this Windows installation can create and attach AppContainers.
    ///
    /// Both the profile API (`userenv!CreateAppContainerProfile`) and the token
    /// API (`kernelbase!CreateAppContainerToken`) have to be present. The
    /// answer is cached; it cannot change within a process.
    #[must_use]
    pub(crate) fn app_container_available() -> bool {
        *AVAILABLE.get_or_init(|| {
            let userenv = load_module("userenv.dll");
            let kernelbase = load_module("kernelbase.dll");
            if userenv.is_null() || kernelbase.is_null() {
                return false;
            }
            let profile =
                unsafe { GetProcAddress(userenv, c"CreateAppContainerProfile".as_ptr().cast()) };
            let token =
                unsafe { GetProcAddress(kernelbase, c"CreateAppContainerToken".as_ptr().cast()) };
            profile.is_some() && token.is_some()
        })
    }

    fn load_module(name: &str) -> windows_sys::Win32::Foundation::HMODULE {
        let wide = wide(name);
        let handle = unsafe { GetModuleHandleW(wide.as_ptr()) };
        if handle.is_null() {
            unsafe { LoadLibraryW(wide.as_ptr()) }
        } else {
            handle
        }
    }

    fn create_app_container_token_fn() -> Option<CreateAppContainerTokenFn> {
        let kernelbase = load_module("kernelbase.dll");
        if kernelbase.is_null() {
            return None;
        }
        let proc =
            unsafe { GetProcAddress(kernelbase, c"CreateAppContainerToken".as_ptr().cast())? };
        // SAFETY: the export has the documented-for-Chromium signature above;
        // it is only ever called with matching arguments.
        Some(unsafe { std::mem::transmute::<RawProc, CreateAppContainerTokenFn>(proc) })
    }

    /// A live AppContainer profile and its SID.
    pub(crate) struct AppContainer {
        /// The stable profile name. The two profiles (network and no network)
        /// have different SIDs, so ACL grants have to be keyed by this and not
        /// just by path.
        profile: &'static str,
        sid: PSID,
        capabilities: CapabilitySids,
    }

    // SAFETY: the SID is allocated by the OS and only read/owned by this
    // handle; moving the handle between threads does not create aliasing.
    unsafe impl Send for AppContainer {}
    // SAFETY: as above; the ACL/profile APIs are called with a stable SID.
    unsafe impl Sync for AppContainer {}

    /// Capability SIDs, kept in buffers whose addresses do not move.
    struct CapabilitySids {
        // The `SID_AND_ATTRIBUTES` entries point into these allocations, so
        // each buffer is boxed to keep its address stable if the outer `Vec`
        // reallocates. That matters more than the extra indirection; the
        // buffers also have to outlive the entries even though the field is
        // not read directly.
        #[allow(dead_code, clippy::vec_box)]
        buffers: Vec<Box<[u8; SECURITY_MAX_SID_SIZE as usize]>>,
        attributes: Vec<SID_AND_ATTRIBUTES>,
    }

    impl CapabilitySids {
        fn from_names(names: &[&str]) -> Result<Self, SandboxError> {
            let mut buffers = Vec::new();
            let mut attributes = Vec::new();
            for name in names {
                if *name != super::INTERNET_CLIENT_CAPABILITY {
                    return Err(SandboxError::IsolationUnavailable(format!(
                        "unsupported AppContainer capability '{name}'"
                    )));
                }
                let mut buffer: Box<[u8; SECURITY_MAX_SID_SIZE as usize]> =
                    Box::new([0; SECURITY_MAX_SID_SIZE as usize]);
                let mut size = SECURITY_MAX_SID_SIZE;
                let ok = unsafe {
                    CreateWellKnownSid(
                        WinCapabilityInternetClientSid,
                        ptr::null_mut(),
                        buffer.as_mut_ptr().cast(),
                        &mut size,
                    )
                };
                if ok == 0 {
                    return Err(last_error("CreateWellKnownSid"));
                }
                let sid = buffer.as_mut_ptr().cast::<c_void>();
                buffers.push(buffer);
                attributes.push(SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: SE_GROUP_ENABLED,
                });
            }
            Ok(Self {
                buffers,
                attributes,
            })
        }

        fn as_ptr(&self) -> *const SID_AND_ATTRIBUTES {
            if self.attributes.is_empty() {
                ptr::null()
            } else {
                self.attributes.as_ptr()
            }
        }

        fn len(&self) -> u32 {
            u32::try_from(self.attributes.len()).unwrap_or(0)
        }
    }

    impl AppContainer {
        /// Create or derive the stable profile and grant the policy paths.
        ///
        /// The profile name is stable across processes on purpose: the SID in
        /// DACLs has to stay the same, or every command would add another ACE
        /// to every file. Two profiles exist because the capability set is
        /// fixed when the profile is created: one without capabilities (the
        /// default) and one with `internetClient` when the network is allowed.
        pub(crate) fn create(config: &SandboxConfig) -> Result<Self, SandboxError> {
            if !app_container_available() {
                return Err(SandboxError::IsolationUnavailable(
                    "AppContainer APIs are not available on this Windows installation".into(),
                ));
            }

            let names = capability_names(config);
            let capabilities = CapabilitySids::from_names(&names)?;
            let profile = if config.allow_network {
                "CodeRex.Sandbox.Network"
            } else {
                "CodeRex.Sandbox"
            };
            let profile_wide = wide(profile);
            let display_wide = wide("CodeRex sandbox");
            let description_wide =
                wide("Filesystem and network isolation for CodeRex subprocesses");

            // Two builds in one process (the runtime tests, or a session that
            // allows the network for one server and not another) can ask for
            // the same stable profile concurrently. The Win32 profile store
            // answers a simultaneous create with E_UNEXPECTED rather than
            // ERROR_ALREADY_EXISTS, so the calls are serialized here and a
            // failed create always falls back to deriving the existing SID.
            //
            // Only the create/derive sequence is under the lock. The ACL plan
            // below is the slow, filesystem-heavy part (it writes a DACL and a
            // mandatory label for every system root, `PATH` entry and workspace
            // file), so holding this lock across it would serialize every
            // sandbox creation in the process behind the slowest one.
            let sid = {
                static PROFILE_LOCK: Mutex<()> = Mutex::new(());
                let _profile_guard = PROFILE_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);

                let mut sid: PSID = ptr::null_mut();
                // SAFETY: every pointer is a live, NUL-terminated UTF-16 string
                // and `sid` is a valid out pointer. A zero-length capability
                // list is passed as null.
                let hr = unsafe {
                    CreateAppContainerProfile(
                        profile_wide.as_ptr(),
                        display_wide.as_ptr(),
                        description_wide.as_ptr(),
                        capabilities.as_ptr(),
                        capabilities.len(),
                        &mut sid,
                    )
                };
                if hr < 0 {
                    // The profile from an earlier CodeRex process is reused,
                    // which is exactly what the stable DACL design needs.
                    let derived = unsafe {
                        DeriveAppContainerSidFromAppContainerName(profile_wide.as_ptr(), &mut sid)
                    };
                    if derived < 0 {
                        let already = (hr as u32 & 0xffff_0000) == 0x8007_0000
                            && (hr as u32 & 0xffff) == ERROR_ALREADY_EXISTS;
                        let detail = if already {
                            format!(
                                "DeriveAppContainerSidFromAppContainerName failed: 0x{derived:08X}"
                            )
                        } else {
                            format!("CreateAppContainerProfile failed: 0x{hr:08X}")
                        };
                        return Err(SandboxError::CreationError(detail));
                    }
                }
                if sid.is_null() {
                    return Err(SandboxError::CreationError(
                        "AppContainer profile returned a null SID".into(),
                    ));
                }
                sid
            };

            let container = Self {
                profile,
                sid,
                capabilities,
            };
            container.apply_acl_plan(config)?;
            Ok(container)
        }

        /// The security capabilities to put in a token or a process attribute.
        fn security_capabilities(&self) -> SECURITY_CAPABILITIES {
            SECURITY_CAPABILITIES {
                AppContainerSid: self.sid,
                Capabilities: self.capabilities.as_ptr().cast_mut(),
                CapabilityCount: self.capabilities.len(),
                Reserved: 0,
            }
        }

        /// Grant the container SID access to the policy paths.
        ///
        /// A policy path the caller named is **required**: if its DACL or its
        /// mandatory integrity label cannot be set, the sandbox is refused
        /// rather than handed out with a boundary that silently does not hold.
        /// Ancestors and the system/`PATH` roots are best-effort, because a
        /// user cannot rewrite `C:\`'s DACL and failing the whole sandbox over
        /// that would be wrong.
        ///
        /// Write grants also lower the path's mandatory integrity label. An
        /// AppContainer token runs at Low integrity, so a Modify ACE alone is
        /// not enough: the default `NO_WRITE_UP` policy on a Medium-integrity
        /// directory blocks the write regardless of the DACL. That is the bug
        /// the Windows runtime job caught as "the granted workspace was not
        /// writable".
        fn apply_acl_plan(&self, config: &SandboxConfig) -> Result<(), SandboxError> {
            let system_roots = system_roots();
            let search_paths = search_path_dirs();
            let plan = acl_plan(config, &system_roots, &search_paths);

            let applied = GRANTED_ROOTS.get_or_init(|| Mutex::new(Vec::new()));
            let mut applied = applied
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            for grant in &plan {
                // Keyed by profile: the two stable profiles have different
                // SIDs, so a path granted to one must still be granted to the
                // other.
                let key = (
                    self.profile,
                    grant.path.clone(),
                    grant.access,
                    grant.recursive,
                );
                if applied.contains(&key) {
                    continue;
                }
                if !grant.path.exists() {
                    // Do not cache: the path may be created later in this
                    // process, and then it should get its grant.
                    tracing::debug!(
                        path = %grant.path.display(),
                        "sandbox: skipping ACL grant for a path that does not exist"
                    );
                    continue;
                }

                #[cfg(test)]
                #[cfg(test)]
                let started = std::time::Instant::now();
                let result = set_grant(&grant.path, self.sid, grant.access).and_then(|()| {
                    if grant.access == AclAccess::Modify {
                        set_mandatory_label(&grant.path)
                    } else {
                        Ok(())
                    }
                });
                // A cold plan used to spend minutes here without saying which
                // path was responsible; the Windows runtime log is the only
                // place this is observable.
                #[cfg(test)]
                if started.elapsed() > std::time::Duration::from_secs(1) {
                    eprintln!(
                        "[acl] {} {:?} took {:.1}s",
                        grant.path.display(),
                        grant.access,
                        started.elapsed().as_secs_f64()
                    );
                }

                if let Err(error) = result {
                    if grant.best_effort {
                        tracing::debug!(
                            path = %grant.path.display(),
                            %error,
                            "sandbox: best-effort ACL grant failed; the path may still be \
                             reachable through ALL APPLICATION PACKAGES"
                        );
                    } else {
                        return Err(SandboxError::IsolationUnavailable(format!(
                            "could not grant the AppContainer access to {}: {error}",
                            grant.path.display()
                        )));
                    }
                } else {
                    tracing::debug!(
                        path = %grant.path.display(),
                        access = ?grant.access,
                        "sandbox: granted the AppContainer access"
                    );
                }

                if grant.recursive {
                    #[cfg(test)]
                    #[cfg(test)]
                    let walked = std::time::Instant::now();
                    walk_and_grant(&grant.path, self.sid, grant.access);
                    #[cfg(test)]
                    if walked.elapsed() > std::time::Duration::from_secs(1) {
                        eprintln!(
                            "[acl] walk {} took {:.1}s",
                            grant.path.display(),
                            walked.elapsed().as_secs_f64()
                        );
                    }
                }
                applied.push(key);
            }
            Ok(())
        }

        /// Replace the primary token of a suspended child with an AppContainer
        /// token, without resuming it.
        ///
        /// The caller must have created the process with `CREATE_SUSPENDED`;
        /// Windows only permits a primary-token replacement while the initial
        /// thread has not run.
        pub(crate) fn restrict_suspended(&self, process: HANDLE) -> Result<(), SandboxError> {
            let create_token = create_app_container_token_fn().ok_or_else(|| {
                SandboxError::IsolationUnavailable(
                    "CreateAppContainerToken is not exported by this Windows installation".into(),
                )
            })?;

            let mut base_token: HANDLE = ptr::null_mut();
            let ok = unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY,
                    &mut base_token,
                )
            };
            if ok == 0 {
                return Err(last_error("OpenProcessToken"));
            }

            let capabilities = self.security_capabilities();
            let mut token: HANDLE = ptr::null_mut();
            // SAFETY: the base token is valid and `capabilities` (including the
            // SID and capability buffers) outlives the call.
            let hr = unsafe { create_token(base_token, &capabilities, &mut token) };
            unsafe { CloseHandle(base_token) };
            // `CreateAppContainerToken` returns an HRESULT: zero is success.
            if hr < 0 {
                return Err(SandboxError::CreationError(format!(
                    "CreateAppContainerToken failed: 0x{hr:08X}"
                )));
            }

            let access = ProcessAccessToken {
                token,
                thread: ptr::null_mut(),
            };
            // SAFETY: `access` is the structure the information class expects
            // and `process` is the suspended child's handle.
            let status = unsafe {
                NtSetInformationProcess(
                    process,
                    PROCESS_ACCESS_TOKEN_CLASS,
                    (&raw const access).cast(),
                    std::mem::size_of_val(&access) as u32,
                )
            };
            unsafe { CloseHandle(token) };
            if status < 0 {
                return Err(nt_error(
                    "NtSetInformationProcess(ProcessAccessToken)",
                    status,
                ));
            }
            Ok(())
        }

        /// Resume a child suspended by `CREATE_SUSPENDED`.
        pub(crate) fn resume(&self, process: HANDLE) -> Result<(), SandboxError> {
            // SAFETY: `process` is a live process handle owned by the caller.
            let status = unsafe { NtResumeProcess(process) };
            if status < 0 {
                return Err(nt_error("NtResumeProcess", status));
            }
            Ok(())
        }

        /// Spawn a process inside the container through the documented
        /// `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
        /// path.
        ///
        /// **Test-only alternative path.** Production (the shell tools and
        /// hooks) spawns with `CREATE_SUSPENDED` and calls
        /// [`Self::restrict_suspended`] instead, because
        /// `std::process::Command` cannot pass a process attribute list on
        /// stable Rust. This method exists so the Windows test can exercise
        /// the documented primitive; nothing outside `cfg(test)` calls it.
        ///
        /// The environment is inherited from this process.
        #[allow(dead_code)] // test-only: production uses `restrict_suspended`
        pub(crate) fn spawn(
            &self,
            program: &Path,
            args: &[OsString],
            cwd: Option<&Path>,
        ) -> Result<ContainedChild, SandboxError> {
            let program_string = program.to_string_lossy();
            let arg_strings: Vec<String> = args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let mut command_line = super::build_command_line(&program_string, &arg_strings)
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<u16>>();
            let program_wide = wide_os(program.as_os_str());
            let cwd_wide = cwd.map(|dir| wide_os(dir.as_os_str()));

            // Size the attribute list, then allocate and initialize it.
            let mut size = 0usize;
            unsafe {
                InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
            }
            if size == 0 {
                return Err(last_error("InitializeProcThreadAttributeList(size)"));
            }
            let mut buffer = vec![0u8; size];
            let list: LPPROC_THREAD_ATTRIBUTE_LIST = buffer.as_mut_ptr().cast();
            let ok = unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) };
            if ok == 0 {
                return Err(last_error("InitializeProcThreadAttributeList"));
            }

            let capabilities = self.security_capabilities();
            // SAFETY: `capabilities` points at live SIDs for the duration of
            // the two calls, and the attribute size is its own.
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    (&raw const capabilities).cast(),
                    std::mem::size_of_val(&capabilities),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if ok == 0 {
                unsafe { DeleteProcThreadAttributeList(list) };
                return Err(last_error("UpdateProcThreadAttribute"));
            }

            let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.lpAttributeList = list;

            let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // `bInheritHandles` is FALSE on purpose. With TRUE, every
            // inheritable handle in this process leaks into the container
            // child, including handles another thread is creating for its own
            // spawn: Rust's std holds a process-wide lock around `CreateProcess`
            // exactly to stop that, and this raw call does not take it. A
            // leaked write end of another caller's pipe outlives this child and
            // blocks that caller's read until the child exits. The security
            // capabilities travel in the attribute list, so no handle has to be
            // inherited for the container to work.
            //
            // SAFETY: all pointers are live for the duration of the call; the
            // command line is mutable because CreateProcessW may modify it.
            let ok = unsafe {
                CreateProcessW(
                    program_wide.as_ptr(),
                    command_line.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                    ptr::null(),
                    cwd_wide.as_ref().map_or(ptr::null(), |wide| wide.as_ptr()),
                    &startup.StartupInfo,
                    &mut process_info,
                )
            };
            unsafe { DeleteProcThreadAttributeList(list) };
            if ok == 0 {
                return Err(last_error("CreateProcessW"));
            }

            Ok(ContainedChild {
                process: process_info.hProcess,
                thread: process_info.hThread,
            })
        }

        /// The profile SID, for the Windows test and diagnostics.
        #[must_use]
        #[allow(dead_code)] // test-only: read by the Windows confinement tests
        pub(crate) fn sid(&self) -> PSID {
            self.sid
        }
    }

    impl Drop for AppContainer {
        fn drop(&mut self) {
            if !self.sid.is_null() {
                // SAFETY: the SID came from CreateAppContainerProfile or
                // DeriveAppContainerSidFromAppContainerName and is freed once.
                unsafe { FreeSid(self.sid) };
            }
        }
    }

    /// A process started by the test-only [`AppContainer::spawn`] path.
    #[allow(dead_code)] // test-only: constructed only by `spawn`
    pub(crate) struct ContainedChild {
        process: HANDLE,
        thread: HANDLE,
    }

    impl ContainedChild {
        /// Wait for the process to exit and return its exit code.
        #[allow(dead_code)] // test-only: only the Windows confinement test waits
        pub(crate) fn wait(&self) -> Result<u32, SandboxError> {
            // Bounded: a test-only helper must fail with a message, not hang
            // the whole job when a contained process never exits.
            // SAFETY: `process` is a live handle owned by this value.
            let wait = unsafe { WaitForSingleObject(self.process, 30_000) };
            if wait == WAIT_TIMEOUT {
                // Do not leave a stuck contained process behind: it can hold
                // inherited handles and outlive the test that started it, and
                // the job timeout would then hide which test actually hung.
                // SAFETY: `process` is a live handle owned by this value.
                unsafe { TerminateProcess(self.process, 1) };
                // Let the termination land before `Drop` closes the handle.
                unsafe { WaitForSingleObject(self.process, 5_000) };
                return Err(SandboxError::CreationError(
                    "the contained process did not exit within 30 seconds".into(),
                ));
            }
            if wait != WAIT_OBJECT_0 {
                return Err(last_error("WaitForSingleObject"));
            }
            let mut code = 0u32;
            // SAFETY: `code` is a valid out pointer.
            let ok = unsafe { GetExitCodeProcess(self.process, &mut code) };
            if ok == 0 {
                return Err(last_error("GetExitCodeProcess"));
            }
            Ok(code)
        }
    }

    impl Drop for ContainedChild {
        fn drop(&mut self) {
            // SAFETY: both handles were returned by CreateProcessW and are
            // closed exactly once.
            unsafe {
                if !self.thread.is_null() {
                    CloseHandle(self.thread);
                }
                if !self.process.is_null() {
                    CloseHandle(self.process);
                }
            }
        }
    }

    /// Whether `dacl` already grants `permissions` to `sid` on the object
    /// itself (an inherit-only ACE does not count).
    /// The `ALL APPLICATION PACKAGES` SID (`S-1-15-2-1`).
    ///
    /// Every AppContainer token is a member of this group, so a directory that
    /// already grants it the access we need does not need our own ACE. That
    /// matters because `SetNamedSecurityInfoW` is hooked by Defender on
    /// managed machines: a cold ACL plan that rewrote every system root took
    /// over eight minutes on the Windows CI runner.
    ///
    /// The SID is created once and kept for the process; it is a tiny, fixed
    /// allocation.
    fn any_package_sid() -> PSID {
        static SID: OnceLock<usize> = OnceLock::new();
        let value = *SID.get_or_init(|| {
            let mut buffer: Box<[u8; SECURITY_MAX_SID_SIZE as usize]> =
                Box::new([0; SECURITY_MAX_SID_SIZE as usize]);
            let mut size = SECURITY_MAX_SID_SIZE;
            // SAFETY: the buffer is `SECURITY_MAX_SID_SIZE` bytes and suitably
            // aligned for a SID, and `size` is a valid in/out parameter.
            let ok = unsafe {
                CreateWellKnownSid(
                    WinBuiltinAnyPackageSid,
                    ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            if ok == 0 {
                #[cfg(test)]
                eprintln!(
                    "[acl] CreateWellKnownSid(AnyPackage) failed: {}",
                    std::io::Error::last_os_error()
                );
                return 0;
            }
            let leaked: &'static mut [u8; SECURITY_MAX_SID_SIZE as usize] = Box::leak(buffer);
            leaked.as_mut_ptr() as usize
        });
        value as PSID
    }

    fn dacl_grants(dacl: *const ACL, sid: PSID, permissions: u32) -> bool {
        let mut size: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `dacl` is a live ACL and `size` is a valid out pointer.
        let ok = unsafe {
            GetAclInformation(
                dacl,
                std::ptr::from_mut(&mut size).cast(),
                std::mem::size_of_val(&size) as u32,
                AclSizeInformation,
            )
        };
        if ok == 0 {
            return false;
        }
        for index in 0..size.AceCount {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: `index` is below the ACE count just read.
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
                continue;
            }
            // SAFETY: the header precedes every ACE body.
            let header = unsafe { &*(ace.cast::<ACE_HEADER>()) };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags & INHERIT_ONLY_ACE != 0
            {
                continue;
            }
            let allowed = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
            let ace_sid = std::ptr::from_ref(&allowed.SidStart).cast::<c_void>() as PSID;
            let any_package = any_package_sid();
            // SAFETY: all three are valid SIDs for the duration of the calls.
            let grants_us = unsafe { EqualSid(ace_sid, sid) } != 0
                || (!any_package.is_null() && unsafe { EqualSid(ace_sid, any_package) } != 0);
            if grants_us && mask_covers(allowed.Mask, permissions) {
                return true;
            }
        }
        false
    }

    /// Apply one ACE to one path.
    fn set_grant(path: &Path, sid: PSID, access: AclAccess) -> Result<(), SandboxError> {
        let wide_path = wide_os(path.as_os_str());
        let mut old_dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        #[cfg(test)]
        let read_started = std::time::Instant::now();
        // SAFETY: the path is NUL-terminated and every out pointer is valid.
        let err = unsafe {
            GetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut old_dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        #[cfg(test)]
        {
            if read_started.elapsed() > std::time::Duration::from_secs(1) {
                eprintln!(
                    "[acl] read {} took {:.1}s",
                    path.display(),
                    read_started.elapsed().as_secs_f64()
                );
            }
        }
        if err != windows_sys::Win32::Foundation::ERROR_SUCCESS {
            return Err(win32_error("GetNamedSecurityInfoW", err));
        }
        // A null DACL grants everyone, the container included. Replacing it
        // with a single ACE could lock the owner out, so leave it alone.
        if old_dacl.is_null() {
            unsafe { LocalFree(descriptor as HLOCAL) };
            return Ok(());
        }

        let permissions = match access {
            // FILE_TRAVERSE | FILE_READ_ATTRIBUTES: reach the object without
            // listing or reading it.
            AclAccess::Traverse => 0x0000_0020 | 0x0000_0080,
            AclAccess::ReadExecute => FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            AclAccess::Modify => {
                FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | FILE_GENERIC_WRITE | 0x0001_0000
                // DELETE
            }
        };

        // Skip the write when the DACL already grants this. Security API
        // calls are slow on managed machines (Defender hooks them), and a
        // later sandbox in another process usually finds the grant an
        // earlier one made.
        if dacl_grants(old_dacl, sid, permissions) {
            // SAFETY: the descriptor is owned by this function.
            unsafe { LocalFree(descriptor as HLOCAL) };
            return Ok(());
        }

        let mut entry: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        entry.grfAccessPermissions = permissions;
        entry.grfAccessMode = GRANT_ACCESS;
        entry.grfInheritance = if access.inheritable() { INHERIT_ALL } else { 0 };
        entry.Trustee = TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast::<u16>(),
        };

        let mut new_dacl: *mut ACL = ptr::null_mut();
        // SAFETY: `entry` is initialized, `old_dacl` lives in `descriptor`,
        // and `new_dacl` is a valid out pointer.
        let err = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
        if err != windows_sys::Win32::Foundation::ERROR_SUCCESS {
            unsafe { LocalFree(descriptor as HLOCAL) };
            return Err(win32_error("SetEntriesInAclW", err));
        }

        #[cfg(test)]
        let write_started = std::time::Instant::now();
        // SAFETY: both ACLs are live and correctly sized for the calls.
        let err = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_ptr().cast_mut(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                new_dacl,
                ptr::null_mut(),
            )
        };
        #[cfg(test)]
        {
            if write_started.elapsed() > std::time::Duration::from_secs(1) {
                eprintln!(
                    "[acl] write {} took {:.1}s",
                    path.display(),
                    write_started.elapsed().as_secs_f64()
                );
            }
        }
        unsafe {
            LocalFree(new_dacl as HLOCAL);
            LocalFree(descriptor as HLOCAL);
        }
        if err != windows_sys::Win32::Foundation::ERROR_SUCCESS {
            return Err(win32_error("SetNamedSecurityInfoW", err));
        }
        Ok(())
    }

    /// Grant access to an existing tree.
    ///
    /// Inheritable ACEs only cover objects created after the grant, so the
    /// workspace as it stands has to be walked. Symlinks are skipped: their
    /// target may be outside the policy paths, and following it would grant
    /// access the policy did not name.
    fn walk_and_grant(root: &Path, sid: PSID, access: AclAccess) {
        let mut seen = 0usize;
        for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter() {
            let Ok(entry) = entry else {
                continue;
            };
            if entry.path() == root {
                continue;
            }
            seen += 1;
            if seen > MAX_ACL_WALK_ENTRIES {
                tracing::warn!(
                    path = %root.display(),
                    limit = MAX_ACL_WALK_ENTRIES,
                    "sandbox: stopping the ACL walk; descendants past this point rely on \
                     inherited ACEs"
                );
                return;
            }
            if entry.file_type().is_symlink() {
                continue;
            }
            let result = set_grant(entry.path(), sid, access).and_then(|()| {
                if access == AclAccess::Modify {
                    set_mandatory_label(entry.path())
                } else {
                    Ok(())
                }
            });
            if let Err(error) = result {
                tracing::debug!(
                    path = %entry.path().display(),
                    %error,
                    "sandbox: could not grant the AppContainer access to a descendant"
                );
            }
        }
    }

    /// Lower a path's mandatory integrity label to Low.
    ///
    /// An AppContainer token runs at Low integrity and the default
    /// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP` policy refuses writes to anything
    /// labelled Medium or higher. A `Modify` ACE for the container SID is
    /// therefore not sufficient for a write path: the DACL says yes and the
    /// integrity policy still says no. This is what the Windows runtime job
    /// reported as "the granted workspace was not writable".
    ///
    /// The SDDL is `S:(ML;;NW;;;LW)`: a SACL with one mandatory-label ACE,
    /// `no-write-up`, Low integrity. Directories mark their ACE inheritable so
    /// files created later are Low as well; the recursive walk handles files
    /// that already exist.
    fn set_mandatory_label(path: &Path) -> Result<(), SandboxError> {
        // `SDDL_REVISION_1`; the SD is allocated by the Win32 call.
        const SDDL_REVISION_1: u32 = 1;
        /// `SECURITY_MANDATORY_LOW_RID`.
        const LOW_INTEGRITY_RID: u32 = 0x1000;

        // Nothing to do when the path is already Low; this is the common case
        // for every sandbox after the first one in a session.
        if mandatory_label_rid(path) == Some(LOW_INTEGRITY_RID) {
            return Ok(());
        }
        let sddl = wide("S:(ML;;NW;;;LW)");
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated, `descriptor` is a valid out
        // pointer, and the size pointer may be null.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            ));
        }

        let wide_path = wide_os(path.as_os_str());
        // SAFETY: the path is NUL-terminated and `descriptor` is a live SD
        // returned by the call above. `LABEL_SECURITY_INFORMATION` selects the
        // SACL, so the DACL is untouched.
        let ok =
            unsafe { SetFileSecurityW(wide_path.as_ptr(), LABEL_SECURITY_INFORMATION, descriptor) };
        let error = if ok == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        // SAFETY: `descriptor` is owned by this function (LocalAlloc).
        unsafe { LocalFree(descriptor as HLOCAL) };
        match error {
            Some(_) => Err(last_error("SetFileSecurityW")),
            None => Ok(()),
        }
    }

    /// `ACCESS_ALLOWED_ACE_TYPE`; windows-sys does not export the constant.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    /// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`; likewise not exported.
    const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;
    /// `INHERIT_ONLY_ACE`: the ACE applies to children only, not the object.
    const INHERIT_ONLY_ACE: u8 = 0x08;

    /// Describe what the container SID can do on `path`, for test failures.
    ///
    /// The Windows runtime job can only be debugged from its logs, so the
    /// confinement tests put this string in their panic message. It separates
    /// the two failure modes:
    ///
    /// - no ACE (or a `set_grant` error): the DACL was never applied;
    /// - ACE present but the integrity label is Medium/absent: the AppContainer
    ///   is blocked by Mandatory Integrity Control, not by the DACL.
    #[must_use]
    #[allow(dead_code)] // called from the Windows runtime tests
    pub(crate) fn diagnose_path(path: &Path, sid: PSID) -> String {
        let scan = ace_for(path, sid, DACL_SECURITY_INFORMATION);
        let label = mandatory_label_rid(path);
        // Traverse is `FILE_TRAVERSE | FILE_READ_ATTRIBUTES`; printing the
        // mask (not just presence) distinguishes an ancestor we granted from
        // an unrelated ACE naming the same SID.
        const FILE_TRAVERSE: u32 = 0x20;
        let ancestors: Vec<String> = path
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(|ancestor| {
                let scan = ace_for(ancestor, sid, DACL_SECURITY_INFORMATION);
                match scan.allow {
                    Some((mask, flags)) => format!(
                        "{}={mask:#010X} flags={flags:#04X}{}{}",
                        ancestor.display(),
                        if mask & FILE_TRAVERSE != 0 {
                            ""
                        } else {
                            " (no-traverse)"
                        },
                        if flags & INHERIT_ONLY_ACE != 0 {
                            " (inherit-only)"
                        } else {
                            ""
                        }
                    ),
                    None => format!("{}=none", ancestor.display()),
                }
            })
            .collect();
        let write_bit = scan.allow.is_some_and(|(mask, flags)| {
            mask & FILE_GENERIC_WRITE != 0 && flags & INHERIT_ONLY_ACE == 0
        });

        format!(
            "path={} exists={} allow={:?} deny={:?} has_write={} label_rid={:?} ancestors=[{}]",
            path.display(),
            path.exists(),
            scan.allow
                .map(|(mask, flags)| format!("0x{mask:08X}/flags=0x{flags:02X}")),
            scan.deny
                .map(|(mask, flags)| format!("0x{mask:08X}/flags=0x{flags:02X}")),
            write_bit,
            label,
            ancestors.join(", ")
        )
    }

    /// What the DACL says about one SID: the first allow ACE and the first
    /// deny ACE, with their flags. `INHERIT_ONLY_ACE` (0x08) matters: such an
    /// ACE does not apply to the object itself.
    #[derive(Debug, Clone, Copy)]
    struct AceScan {
        allow: Option<(u32, u8)>,
        deny: Option<(u32, u8)>,
    }

    /// `ACCESS_DENIED_ACE_TYPE`; windows-sys does not export the constant.
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;

    /// Scan the DACL for ACEs naming `sid`.
    fn ace_for(path: &Path, sid: PSID, information: u32) -> AceScan {
        let wide_path = wide_os(path.as_os_str());
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: the path is NUL-terminated; `dacl` and `descriptor` are
        // valid out pointers. The DACL argument is reused for the SACL when
        // `information` asks for a label.
        let error = unsafe {
            GetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                information,
                ptr::null_mut(),
                ptr::null_mut(),
                if information == DACL_SECURITY_INFORMATION {
                    &mut dacl
                } else {
                    ptr::null_mut()
                },
                if information == DACL_SECURITY_INFORMATION {
                    ptr::null_mut()
                } else {
                    &mut dacl
                },
                &mut descriptor,
            )
        };
        if error != windows_sys::Win32::Foundation::ERROR_SUCCESS || dacl.is_null() {
            if !descriptor.is_null() {
                // SAFETY: the descriptor is owned by this function.
                unsafe { LocalFree(descriptor as HLOCAL) };
            }
            return AceScan {
                allow: None,
                deny: None,
            };
        }

        let mut size: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `dacl` is a live ACL and `size` is a valid out pointer.
        let ok = unsafe {
            GetAclInformation(
                dacl,
                std::ptr::from_mut(&mut size).cast(),
                std::mem::size_of_val(&size) as u32,
                AclSizeInformation,
            )
        };
        let mut scan = AceScan {
            allow: None,
            deny: None,
        };
        if ok != 0 {
            for index in 0..size.AceCount {
                let mut ace: *mut c_void = ptr::null_mut();
                // SAFETY: `index` is below the ACE count just read.
                if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
                    continue;
                }
                // SAFETY: an access-allowed ACE begins with its header; the
                // SID starts at `SidStart` for both shapes inspected here.
                let header = unsafe { &*(ace.cast::<ACE_HEADER>()) };
                if header.AceType != ACCESS_ALLOWED_ACE_TYPE
                    && header.AceType != ACCESS_DENIED_ACE_TYPE
                {
                    continue;
                }
                let allowed = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
                let ace_sid = std::ptr::from_ref(&allowed.SidStart).cast::<c_void>() as PSID;
                // SAFETY: both are valid SIDs for the duration of the call.
                if unsafe { EqualSid(ace_sid, sid) } != 0 {
                    let entry = (allowed.Mask, header.AceFlags);
                    if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
                        if scan.allow.is_none() {
                            scan.allow = Some(entry);
                        }
                    } else if scan.deny.is_none() {
                        scan.deny = Some(entry);
                    }
                }
            }
        }

        // SAFETY: the descriptor is owned by this function; the ACL points
        // into it.
        unsafe { LocalFree(descriptor as HLOCAL) };
        scan
    }

    /// The mandatory integrity label RID (0x1000 Low, 0x2000 Medium, …).
    fn mandatory_label_rid(path: &Path) -> Option<u32> {
        let wide_path = wide_os(path.as_os_str());
        let mut sacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: the path is NUL-terminated and the out pointers are valid.
        let error = unsafe {
            GetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut sacl,
                &mut descriptor,
            )
        };
        if error != windows_sys::Win32::Foundation::ERROR_SUCCESS || sacl.is_null() {
            if !descriptor.is_null() {
                // SAFETY: the descriptor is owned by this function.
                unsafe { LocalFree(descriptor as HLOCAL) };
            }
            return None;
        }

        let mut size: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `sacl` is a live ACL and `size` is a valid out pointer.
        let ok = unsafe {
            GetAclInformation(
                sacl,
                std::ptr::from_mut(&mut size).cast(),
                std::mem::size_of_val(&size) as u32,
                AclSizeInformation,
            )
        };
        let mut found = None;
        if ok != 0 {
            for index in 0..size.AceCount {
                let mut ace: *mut c_void = ptr::null_mut();
                // SAFETY: `index` is below the ACE count just read.
                if unsafe { GetAce(sacl, index, &mut ace) } == 0 || ace.is_null() {
                    continue;
                }
                // SAFETY: the header precedes every ACE body.
                let header = unsafe { &*(ace.cast::<ACE_HEADER>()) };
                if header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE {
                    continue;
                }
                let label = unsafe { &*(ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>()) };
                let label_sid = std::ptr::from_ref(&label.SidStart).cast::<c_void>() as PSID;
                // SAFETY: the SID is part of the ACE; a label SID always has
                // at least one subauthority.
                let count = unsafe { *GetSidSubAuthorityCount(label_sid) };
                if count == 0 {
                    continue;
                }
                // SAFETY: `count - 1` is a valid subauthority index.
                let rid = unsafe { *GetSidSubAuthority(label_sid, u32::from(count - 1)) };
                found = Some(rid);
                break;
            }
        }

        // SAFETY: the descriptor is owned by this function.
        unsafe { LocalFree(descriptor as HLOCAL) };
        found
    }

    /// The system roots a shell and its loader need.
    fn system_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        roots.push(system_root.clone());
        roots.push(system_root.join("System32"));
        roots.push(system_root.join("SysWOW64"));
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(value) = std::env::var_os(variable) {
                roots.push(PathBuf::from(value));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Microsoft").join("WindowsApps"));
        }
        dedup(roots)
    }

    /// The directories on `PATH`, which hold the shell and its toolchain.
    fn search_path_dirs() -> Vec<PathBuf> {
        let Some(path) = std::env::var_os("PATH") else {
            return Vec::new();
        };
        dedup(
            path.to_string_lossy()
                .split(';')
                .filter(|entry| !entry.trim().is_empty())
                .map(PathBuf::from)
                .collect(),
        )
    }

    fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let mut unique = Vec::with_capacity(paths.len());
        for path in paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        unique
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn wide_os(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn last_error(operation: &str) -> SandboxError {
        let error = std::io::Error::last_os_error();
        SandboxError::CreationError(format!("{operation} failed: {error}"))
    }

    fn win32_error(operation: &str, code: u32) -> SandboxError {
        SandboxError::CreationError(format!(
            "{operation} failed: {}",
            std::io::Error::from_raw_os_error(code as i32)
        ))
    }

    fn nt_error(operation: &str, status: i32) -> SandboxError {
        let code = unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(status) };
        SandboxError::CreationError(format!(
            "{operation} failed: NTSTATUS 0x{status:08X} ({})",
            std::io::Error::from_raw_os_error(code as i32)
        ))
    }
}

#[cfg(target_os = "windows")]
pub(crate) use imp::AppContainer;

/// Whether the AppContainer runtime is available — always false off Windows.
#[cfg(not(target_os = "windows"))]
#[must_use]
pub(crate) const fn app_container_available() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub(crate) use imp::app_container_available;

/// Describe a path's DACL and mandatory label for a failure message.
///
/// Windows only: the values it reports do not exist elsewhere.
#[cfg(target_os = "windows")]
/// Only the confinement tests call this (via `PlatformSandbox::diagnose`).
#[cfg(test)]
pub(crate) use imp::diagnose_path;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SandboxConfig {
        SandboxConfig {
            // Forward slashes so `Path::ancestors` behaves the same on the
            // macOS host these pure tests run on as it does on Windows.
            allowed_read_paths: vec![PathBuf::from("C:/work/project")],
            allowed_write_paths: vec![PathBuf::from("C:/work/project")],
            allowed_exec_paths: vec![PathBuf::from("C:/tools/bin")],
            additional_working_dirs: vec![PathBuf::from("C:/work/project")],
            allow_network: false,
            ..SandboxConfig::default()
        }
    }

    fn plan() -> Vec<AclGrant> {
        acl_plan(
            &config(),
            &[PathBuf::from("C:/Windows")],
            &[
                PathBuf::from("C:/tools/bin"),
                PathBuf::from("C:/Windows/System32/WindowsPowerShell/v1.0"),
            ],
        )
    }

    fn grant_for<'a>(plan: &'a [AclGrant], path: &str) -> &'a AclGrant {
        plan.iter()
            .find(|grant| grant.path.as_path() == std::path::Path::new(path))
            .unwrap_or_else(|| panic!("{path} should be granted"))
    }

    #[test]
    fn acl_plan_grants_read_execute_on_system_and_search_paths() {
        let plan = plan();
        for path in ["C:/Windows", "C:/Windows/System32/WindowsPowerShell/v1.0"] {
            let grant = grant_for(&plan, path);
            assert_eq!(grant.access, AclAccess::ReadExecute);
            assert!(!grant.recursive);
            assert!(grant.best_effort, "{path} should be best-effort");
        }
    }

    /// Best-effort roots already rely on the platform granting the container
    /// access, so they do not spend a DACL write on every ancestor. On the CI
    /// runner those ancestor writes were where the 480 seconds went.
    #[test]
    fn best_effort_roots_do_not_grant_their_ancestors() {
        let plan = plan();
        for ancestor in [
            "C:/Windows/System32/WindowsPowerShell",
            "C:/Windows/System32",
        ] {
            assert!(
                !plan
                    .iter()
                    .any(|grant| grant.path == std::path::Path::new(ancestor)),
                "{ancestor} is an ancestor of a search path and should not be granted"
            );
        }
    }

    /// Policy paths are the ones the container cannot otherwise reach, so
    /// their ancestor chain is still granted — except a drive root, which
    /// already grants ALL APPLICATION PACKAGES and costs minutes to rewrite.
    #[test]
    fn policy_paths_still_get_their_ancestor_traverse_grants() {
        let plan = plan();
        let grant = grant_for(&plan, "C:/work");
        assert_eq!(grant.access, AclAccess::Traverse);
        assert!(
            !plan
                .iter()
                .any(|grant| grant.path == std::path::Path::new("C:/")),
            "a drive root must never be rewritten"
        );
    }

    /// The generic bits stand for specific rights, so an ACE that grants
    /// `GENERIC_EXECUTE` already grants traverse and read-attributes.
    #[test]
    fn generic_ace_bits_cover_the_specific_rights_they_stand_for() {
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_EXECUTE: u32 = 0x2000_0000;
        const GENERIC_ALL: u32 = 0x1000_0000;
        const FILE_TRAVERSE: u32 = 0x0000_0020;
        const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
        let traverse = FILE_TRAVERSE | FILE_READ_ATTRIBUTES;

        assert!(mask_covers(traverse, traverse), "the specific bits match");
        assert!(mask_covers(GENERIC_EXECUTE, traverse));
        assert!(
            !mask_covers(GENERIC_READ, traverse),
            "GENERIC_READ covers read-attributes but not FILE_TRAVERSE, \
             which is the directory form of FILE_EXECUTE"
        );
        assert!(mask_covers(GENERIC_ALL, traverse));
        assert!(!mask_covers(0, traverse));
        assert!(
            !mask_covers(FILE_TRAVERSE, traverse),
            "traverse alone does not grant read-attributes"
        );
    }

    /// A path named by the policy is required even when it is also on `PATH`.
    #[test]
    fn acl_plan_treats_a_configured_exec_path_as_required() {
        let plan = plan();
        let grant = grant_for(&plan, "C:/tools/bin");
        assert_eq!(grant.access, AclAccess::ReadExecute);
        assert!(!grant.best_effort);
        assert!(grant.recursive, "an exec tree may already contain files");
    }

    #[test]
    fn acl_plan_grants_read_write_on_the_workspace() {
        let plan = plan();
        let grant = grant_for(&plan, "C:/work/project");
        assert_eq!(grant.access, AclAccess::Modify);
        assert!(grant.recursive, "existing files need the ACL walk");
        assert!(!grant.best_effort, "a policy path must be granted");
    }

    /// The user's whole temp tree is never granted: a sandbox gets a private
    /// temp directory named in its own policy instead.
    #[test]
    fn acl_plan_does_not_grant_the_user_temp_tree() {
        let plan = plan();
        assert!(
            !plan
                .iter()
                .any(|grant| grant.path == std::path::Path::new("C:/Users/me/AppData/Local/Temp")),
            "the shared temp tree must not be granted"
        );
    }

    /// Ancestors are traverse-only and best-effort, so a user who cannot
    /// rewrite `C:\`'s DACL does not lose the whole sandbox.
    #[test]
    fn acl_plan_treats_ancestors_as_best_effort() {
        let plan = plan();
        for path in ["C:/work", "C:/tools"] {
            let grant = grant_for(&plan, path);
            assert_eq!(grant.access, AclAccess::Traverse);
            assert!(
                grant.best_effort,
                "{path} is an ancestor; failing to grant it must not abort the sandbox"
            );
        }
    }

    /// `canonicalize` produces verbatim paths; the plan must fold them back so
    /// one path does not get two ACEs (one of them on `\\?\C:\`).
    #[test]
    fn acl_plan_normalizes_verbatim_paths() {
        let mut config = config();
        config.allowed_write_paths = vec![PathBuf::from(r"\\?\C:\work\project")];
        let plan = acl_plan(&config, &[], &[]);
        let grant = grant_for(&plan, "C:/work/project");
        assert_eq!(grant.access, AclAccess::Modify);
        assert!(
            !plan
                .iter()
                .any(|g| g.path.to_string_lossy().starts_with(r"\\?\")),
            "verbatim paths must not survive into the plan"
        );
    }

    /// An AppContainer token has no bypass-traverse privilege.
    #[test]
    fn acl_plan_grants_traverse_on_ancestors() {
        let plan = plan();
        for path in ["C:/work", "C:/tools"] {
            let grant = grant_for(&plan, path);
            assert_eq!(grant.access, AclAccess::Traverse);
            assert!(!grant.recursive, "traverse must not widen the subtree");
        }
    }

    /// Traverse is weaker than read/execute, so merging must not downgrade.
    #[test]
    fn acl_plan_keeps_the_strongest_access_for_a_shared_path() {
        let plan = plan();
        // C:/Windows is both a system root and an ancestor of a search path.
        let grant = grant_for(&plan, "C:/Windows");
        assert_eq!(grant.access, AclAccess::ReadExecute);
    }

    /// The workspace appears in three fields; it must not produce three ACEs.
    #[test]
    fn acl_plan_deduplicates_and_upgrades() {
        let plan = plan();
        let project = PathBuf::from("C:/work/project");
        let entries: Vec<&AclGrant> = plan.iter().filter(|grant| grant.path == project).collect();
        assert_eq!(entries.len(), 1, "duplicate ACL entries: {entries:?}");
        assert_eq!(entries[0].access, AclAccess::Modify);
    }

    #[test]
    fn no_network_means_no_capabilities() {
        assert!(capability_names(&config()).is_empty());
    }

    #[test]
    fn network_permission_grants_only_internet_client() {
        let mut config = config();
        config.allow_network = true;
        assert_eq!(capability_names(&config), vec!["internetClient"]);
    }

    #[test]
    fn command_line_quotes_arguments_with_spaces() {
        let line = build_command_line(
            r"C:\Program Files\PowerShell\pwsh.exe",
            &["-Command".to_string(), "Write-Output hi".to_string()],
        );
        assert_eq!(
            line,
            r#""C:\Program Files\PowerShell\pwsh.exe" -Command "Write-Output hi""#
        );
    }

    #[test]
    fn command_line_leaves_plain_arguments_alone() {
        assert_eq!(
            build_command_line("cmd.exe", &["/C".to_string(), "echo".to_string()]),
            "cmd.exe /C echo"
        );
    }

    #[test]
    fn command_line_escapes_quotes_and_trailing_backslashes() {
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(
            quote_windows_arg(r"C:\dir with space\"),
            r#""C:\dir with space\\""#
        );
        assert_eq!(quote_windows_arg(""), r#""""#);
    }

    #[test]
    fn acl_plan_is_stable_order_for_the_same_input() {
        assert_eq!(plan(), plan());
    }

    #[cfg(target_os = "windows")]
    mod windows {
        use super::*;
        use std::ffi::OsString;

        /// A scratch tree outside `%TEMP%`, which the sandbox always grants.
        fn scratch(name: &str) -> PathBuf {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            base.join(format!(
                "coderex-appcontainer-{}-{name}",
                std::process::id()
            ))
        }

        /// The documented `STARTUPINFOEX` path, exercised on Windows.
        ///
        /// The container can write inside the granted directory and cannot
        /// write into a sibling `denied` directory that was never granted.
        #[test]
        fn spawn_in_container_confines_a_real_process() {
            if !app_container_available() {
                assert!(
                    std::env::var("CODEREX_REQUIRE_APP_CONTAINER").as_deref() != Ok("1"),
                    "AppContainer is unavailable on this runner but \
                     CODEREX_REQUIRE_APP_CONTAINER=1"
                );
                eprintln!("skipping: AppContainer APIs are not available");
                return;
            }

            let root = scratch("spawn");
            let allowed = root.join("allowed");
            let denied = root.join("denied");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&allowed).unwrap();
            std::fs::create_dir_all(&denied).unwrap();

            let config = SandboxConfig {
                allowed_read_paths: vec![allowed.clone()],
                allowed_write_paths: vec![allowed.clone()],
                additional_working_dirs: vec![allowed.clone()],
                ..SandboxConfig::default()
            };
            let container = AppContainer::create(&config).unwrap();
            assert!(!container.sid().is_null());

            let system_root =
                std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
            let cmd = PathBuf::from(system_root).join("System32").join("cmd.exe");
            let script = format!(
                "echo inside> {} & echo outside> {}",
                allowed.join("inside.txt").display(),
                denied.join("outside.txt").display()
            );
            let child = match container.spawn(
                &cmd,
                &[OsString::from("/C"), OsString::from(script)],
                Some(&allowed),
            ) {
                Ok(child) => child,
                Err(error) => panic!(
                    "the container should spawn cmd.exe ({error}); {}",
                    diagnose_path(&cmd, container.sid())
                ),
            };
            let _ = child.wait();

            let acl = std::process::Command::new("icacls")
                .arg(&allowed)
                .output()
                .map_or_else(
                    |error| format!("<icacls failed: {error}>"),
                    |out| String::from_utf8_lossy(&out.stdout).into_owned(),
                );
            assert!(
                allowed.join("inside.txt").exists(),
                "the granted workspace was not writable; {}\nicacls:\n{acl}",
                diagnose_path(&allowed, container.sid())
            );
            assert!(
                !denied.join("outside.txt").exists(),
                "a path outside the policy was writable"
            );

            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
