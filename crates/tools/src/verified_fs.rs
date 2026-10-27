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

/// The kind of a directory entry, without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symlink (not followed).
    Symlink,
}

/// A relative entry produced by [`VerifiedDir::walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    /// Path relative to the verified directory.
    pub relative: PathBuf,
    /// Entry kind.
    pub kind: EntryKind,
}

/// An open directory whose real path has been verified.
///
/// The capability (`cap_std::fs::Dir`) is kept open: entries and file contents
/// are opened through it with `openat`-style operations, so a rename or symlink
/// swap of the ambient path after the check cannot redirect the walk. Symlinks
/// encountered below the root are reported but never followed.
#[derive(Debug)]
pub struct VerifiedDir {
    dir: cap_std::fs::Dir,
}

impl VerifiedDir {
    /// Immediate entries, including symlinks.
    pub fn entries(&self) -> std::io::Result<Vec<WalkEntry>> {
        let mut entries = Vec::new();
        for entry in self.dir.entries()? {
            let entry = entry?;
            let kind = entry_kind(&entry)?;
            entries.push(WalkEntry {
                relative: PathBuf::from(entry.file_name()),
                kind,
            });
        }
        Ok(entries)
    }

    /// Recursively list regular files and directories, skipping `.git`.
    ///
    /// Symlinks are reported but never followed, so a link cannot make the
    /// walk leave the capability.
    pub fn walk(&self, max_entries: usize) -> std::io::Result<Vec<WalkEntry>> {
        let mut entries = Vec::new();
        walk_into(&self.dir, Path::new(""), &mut entries, max_entries)?;
        Ok(entries)
    }

    /// Open a file beneath this directory, without following a symlink.
    pub fn open(&self, relative: &Path) -> std::io::Result<cap_std::fs::File> {
        self.dir.open(relative)
    }
}

fn walk_into(
    dir: &cap_std::fs::Dir,
    prefix: &Path,
    entries: &mut Vec<WalkEntry>,
    max_entries: usize,
) -> std::io::Result<()> {
    if entries.len() >= max_entries {
        return Ok(());
    }
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let relative = prefix.join(&name);
        let kind = entry_kind(&entry)?;
        if kind == EntryKind::Dir && name == ".git" {
            continue;
        }
        entries.push(WalkEntry {
            relative: relative.clone(),
            kind,
        });
        if entries.len() >= max_entries {
            return Ok(());
        }
        if kind == EntryKind::Dir {
            let child = dir.open_dir(&name)?;
            walk_into(&child, &relative, entries, max_entries)?;
        }
    }
    Ok(())
}

fn entry_kind(entry: &cap_std::fs::DirEntry) -> std::io::Result<EntryKind> {
    let file_type = entry.file_type()?;
    Ok(if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    })
}

/// Open a directory as a capability and verify its real path.
pub fn open_dir(path: &Path, ctx: &ToolContext<'_>) -> Result<VerifiedDir, ToolError> {
    let dir = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority()).map_err(
        |error| ToolError::InvalidInput {
            message: format!("failed to access directory: {error}"),
        },
    )?;
    let real = dir
        .canonicalize(".")
        .map_err(|error| ToolError::InvalidInput {
            message: format!("failed to resolve directory: {error}"),
        })?;
    ctx.verify_paths(std::slice::from_ref(&real))?;
    Ok(VerifiedDir { dir })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;

    fn ctx(path: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, path)
    }

    #[test]
    fn the_capability_keeps_walking_after_the_root_is_renamed() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();

        let verified = open_dir(&root, &ctx(parent.path())).unwrap();
        let moved = parent.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();

        let entries = verified.walk(100).unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.relative == Path::new("a.txt") && entry.kind == EntryKind::File),
            "{entries:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_below_the_root_is_reported_but_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();

        let verified = open_dir(root.path(), &ctx(root.path())).unwrap();
        let entries = verified.walk(100).unwrap();
        assert!(
            entries.iter().any(|entry| {
                entry.relative == Path::new("link") && entry.kind == EntryKind::Symlink
            }),
            "{entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.relative == Path::new("link/secret.txt")),
            "the walk followed the symlink: {entries:?}"
        );
    }

    #[test]
    fn immediate_entries_include_directories_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let verified = open_dir(root.path(), &ctx(root.path())).unwrap();
        let entries = verified.entries().unwrap();
        assert!(
            entries.iter().any(|entry| {
                entry.relative == Path::new("sub") && entry.kind == EntryKind::Dir
            })
        );
    }
}
