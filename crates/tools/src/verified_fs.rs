//! Open a file and verify the path the OS actually resolved.
//!
//! Checking a path string before a tool runs is a TOCTOU snapshot: a symlink
//! can be retargeted between the check and the open. These helpers return the
//! file handle together with the handle's real path, and the tool asks the
//! session to verify that path before reading or writing through the same
//! handle. A retargeted link therefore cannot make the tool touch a file the
//! permission check never saw.

// `fcntl(F_GETPATH)` and `GetFinalPathNameByHandleW` are the only portable
// ways to ask the OS for an open handle's path; every call site carries a
// SAFETY note.
#![allow(unsafe_code)]

use crate::context::ToolError;
use crate::tool::ToolContext;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// A file handle whose real path has been verified.
#[derive(Debug)]
pub struct VerifiedFile {
    /// The open handle; all reads/writes go through it.
    pub file: File,
    /// The path the OS resolved for the handle.
    pub path: PathBuf,
    /// Whether this call created the file.
    pub created: bool,
}

/// Open an existing file for reading and verify its real path.
pub fn open_read(path: &Path, ctx: &ToolContext<'_>) -> Result<VerifiedFile, ToolError> {
    let file = File::open(path).map_err(|error| ToolError::InvalidInput {
        message: format!("failed to read file: {error}"),
    })?;
    verified(file, path, ctx, false)
}

/// Open (or create) a file for writing, without truncating it yet.
pub fn open_write(path: &Path, ctx: &ToolContext<'_>) -> Result<VerifiedFile, ToolError> {
    // `create_new` tells us reliably whether this call created the file; the
    // fallback opens an existing target (including through a final symlink).
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => verified(file, path, ctx, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new()
                .write(true)
                .truncate(false)
                .open(path)
                .map_err(|error| ToolError::InvalidInput {
                    message: format!("failed to write file: {error}"),
                })?;
            verified(file, path, ctx, false)
        }
        Err(error) => Err(ToolError::InvalidInput {
            message: format!("failed to write file: {error}"),
        }),
    }
}

/// Open an existing file for reading and writing.
pub fn open_read_write(path: &Path, ctx: &ToolContext<'_>) -> Result<VerifiedFile, ToolError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| ToolError::InvalidInput {
            message: format!("failed to open file: {error}"),
        })?;
    verified(file, path, ctx, false)
}

fn verified(
    file: File,
    original: &Path,
    ctx: &ToolContext<'_>,
    created: bool,
) -> Result<VerifiedFile, ToolError> {
    let resolved = real_path(&file, original);
    ctx.verify_paths(std::slice::from_ref(&resolved))?;
    Ok(VerifiedFile {
        file,
        path: resolved,
        created,
    })
}

/// The path the OS resolved for an open handle, falling back to canonicalizing
/// the caller's spelling when the platform cannot report it.
fn real_path(file: &File, original: &Path) -> PathBuf {
    fd_path(file).unwrap_or_else(|| {
        std::fs::canonicalize(original).unwrap_or_else(|_| original.to_path_buf())
    })
}

#[cfg(target_os = "linux")]
fn fd_path(file: &File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    let link = format!("/proc/self/fd/{}", file.as_raw_fd());
    let path = std::fs::read_link(link).ok()?;
    // A deleted-but-open file reports a " (deleted)" suffix.
    let text = path.to_string_lossy();
    Some(PathBuf::from(
        text.strip_suffix(" (deleted)").unwrap_or(&text),
    ))
}

#[cfg(target_os = "macos")]
fn fd_path(file: &File) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::io::AsRawFd;
    let mut buffer = [0_i8; libc::PATH_MAX as usize];
    // SAFETY: `buffer` is PATH_MAX bytes and the fd is valid for the handle;
    // F_GETPATH writes a NUL-terminated path into it.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
    if result == -1 {
        return None;
    }
    let path = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
    Some(PathBuf::from(path.into_owned()))
}

#[cfg(windows)]
fn fd_path(file: &File) -> Option<PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let handle = file.as_raw_handle() as isize;
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: the handle is valid and `buffer` is a writable UTF-16 buffer of
    // the declared length.
    let length =
        unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if length == 0 || length as usize >= buffer.len() {
        return None;
    }
    let text = String::from_utf16_lossy(&buffer[..length as usize]);
    let text = text.strip_prefix(r"\\?\UNC\").map_or_else(
        || text.trim_start_matches(r"\\?\").to_string(),
        |rest| format!(r"\\{rest}"),
    );
    Some(PathBuf::from(text))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn fd_path(_file: &File) -> Option<PathBuf> {
    None
}
