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
    // The parent is opened as a capability first, so a rename or symlink swap
    // between creating the file and verifying its handle cannot make cleanup
    // delete a different file: removal goes through the same directory handle.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| ToolError::InvalidInput {
        message: format!("not a file path: {}", path.display()),
    })?;
    let dir = cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority()).map_err(
        |error| ToolError::InvalidInput {
            message: format!("failed to access directory: {error}"),
        },
    )?;

    let mut create = cap_std::fs::OpenOptions::new();
    create.write(true).create_new(true);
    match dir.open_with(name, &create) {
        Ok(file) => verified_created(file.into_std(), path, ctx, &dir, name),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut open = cap_std::fs::OpenOptions::new();
            open.write(true).truncate(false);
            let file = dir
                .open_with(name, &open)
                .map_err(|error| ToolError::InvalidInput {
                    message: format!("failed to write file: {error}"),
                })?;
            verified(file.into_std(), path, ctx, false)
        }
        Err(error) => Err(ToolError::InvalidInput {
            message: format!("failed to write file: {error}"),
        }),
    }
}

/// Verify a newly created file, removing it through the parent capability if
/// the verification fails. The removal is relative to the same open directory
/// handle that created the file, so it cannot race with a path swap.
fn verified_created(
    file: File,
    original: &Path,
    ctx: &ToolContext<'_>,
    dir: &cap_std::fs::Dir,
    name: &std::ffi::OsStr,
) -> Result<VerifiedFile, ToolError> {
    let resolved = real_path(&file, original);
    if let Err(error) = ctx.verify_paths(std::slice::from_ref(&resolved)) {
        if let Err(cleanup) = dir.remove_file(name) {
            tracing::warn!(
                path = %resolved.display(),
                %cleanup,
                "could not remove a newly created file after verification failed"
            );
        }
        return Err(error);
    }
    Ok(VerifiedFile {
        file,
        path: resolved,
        created: true,
    })
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

    // `windows-sys` 0.59 spells `HANDLE` as `*mut c_void`, so the raw handle
    // is passed through as-is; casting it to `isize` no longer compiles.
    let handle = file.as_raw_handle();
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
    use std::sync::Arc;

    fn ctx(path: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, path)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_handle_path_is_the_symlink_target_not_the_link() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real.txt");
        std::fs::write(&real, "content").unwrap();
        let link = root.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let seen = Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
        let recorder = Arc::clone(&seen);
        let ctx = ToolContext::new(ExecutionContext::Normal, root.path()).with_path_verifier(
            Arc::new(move |paths| {
                recorder.lock().unwrap().extend_from_slice(paths);
                Ok(())
            }),
        );

        let verified = open_read(&link, &ctx).unwrap();
        assert_eq!(verified.path, std::fs::canonicalize(&real).unwrap());
        assert_eq!(
            seen.lock().unwrap().first().map(PathBuf::as_path),
            Some(verified.path.as_path())
        );
    }

    #[test]
    fn a_rejected_new_file_is_removed_through_the_parent_capability() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("new.txt");
        let ctx = ToolContext::new(ExecutionContext::Normal, root.path())
            .with_path_verifier(Arc::new(|_paths| Err("denied by test".to_string())));

        let error = open_write(&target, &ctx).expect_err("the verifier must reject the file");
        assert!(matches!(error, ToolError::Io(_)), "{error:?}");
        assert!(!target.exists(), "the rejected file was left behind");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "race soak; run scripts/race-soak.sh"]
    fn symlink_and_rename_churn_never_reads_outside_the_verified_root() {
        use std::io::Read as _;
        use std::sync::atomic::{AtomicBool, Ordering};

        let base = tempfile::tempdir().unwrap();
        let inside = base.path().join("inside");
        let outside = base.path().join("outside");
        std::fs::create_dir(&inside).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(inside.join("file.txt"), "inside").unwrap();
        std::fs::write(outside.join("file.txt"), "outside-secret").unwrap();

        let gate = base.path().join("current");
        std::os::unix::fs::symlink(&inside, &gate).unwrap();
        let canonical_inside = std::fs::canonicalize(&inside).unwrap();
        let reject_outside = canonical_inside;
        let ctx = ToolContext::new(ExecutionContext::Normal, base.path()).with_path_verifier(
            Arc::new(move |paths| {
                for path in paths {
                    if !path.starts_with(&reject_outside) {
                        return Err(format!("outside root: {}", path.display()));
                    }
                }
                Ok(())
            }),
        );

        let iterations: usize = std::env::var("RACE_SOAK_ITERATIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2_000);
        let stop = Arc::new(AtomicBool::new(false));
        // More than one attacker thread widens the window between the check
        // and the open: one thread is a narrow race, several keep the gate
        // flipping while the reader works.
        let attackers: usize = std::env::var("RACE_SOAK_ATTACKERS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|count| *count > 0)
            .unwrap_or(1);
        let mut threads = Vec::with_capacity(attackers);
        for attacker_index in 0..attackers {
            let attacker_stop = Arc::clone(&stop);
            let attacker_base = base.path().to_path_buf();
            let attacker_gate = gate.clone();
            threads.push(std::thread::spawn(move || {
                let mut index = 0_u64;
                while !attacker_stop.load(Ordering::Relaxed) {
                    let target = if index.is_multiple_of(2) {
                        attacker_base.join("outside")
                    } else {
                        attacker_base.join("inside")
                    };
                    let temporary = attacker_base.join(format!("link-{attacker_index}-{index}"));
                    let _ = std::fs::remove_file(&temporary);
                    if std::os::unix::fs::symlink(&target, &temporary).is_ok() {
                        let _ = std::fs::rename(&temporary, &attacker_gate);
                    }
                    index += 1;
                }
            }));
        }

        let mut successes = 0_usize;
        for _ in 0..iterations {
            let file = gate.join("file.txt");
            if let Ok(mut handle) = open_read(&file, &ctx) {
                let mut content = String::new();
                handle.file.read_to_string(&mut content).unwrap();
                assert!(
                    !content.contains("secret"),
                    "read outside content through the gate: {content}"
                );
                successes += 1;
            }
            if let Ok(directory) = open_dir(&gate, &ctx) {
                for entry in directory.walk(100).unwrap() {
                    if entry.relative != Path::new("file.txt") {
                        continue;
                    }
                    let mut handle = directory.open(&entry.relative).unwrap();
                    let mut content = String::new();
                    handle.read_to_string(&mut content).unwrap();
                    assert!(
                        !content.contains("secret"),
                        "walked outside content through the gate: {content}"
                    );
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(successes > 0, "the gate was never observed pointing inside");
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
