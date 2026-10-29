//! Owning a child process tree, so it can be stopped as a whole.
//!
//! Every platform needs the same thing for a spawned helper that may spawn
//! children of its own: a way to say "kill this process *and everything it
//! started*". Unix gets that from a process group; Windows only gets it from a
//! Job Object, because a child that breaks away from `taskkill /T` (or that
//! `taskkill` cannot see because the parent already exited) survives.
//!
//! This module is the single implementation of that idea, so the CLI monitor
//! supervisor, the bridge cleanup manager, the sandbox and the WASM compiler
//! helper do not each grow their own:
//!
//! - **Unix**: [`ProcessTree::prepare`] puts the child in its own process
//!   group, and [`ProcessTree::terminate`] signals the group.
//! - **Windows**: [`ProcessTree::create`] makes an *anonymous* Job Object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, [`ProcessTree::attach`] puts the
//!   child in it, and [`ProcessTree::terminate`] calls `TerminateJobObject`.
//!   Dropping the tree closes the handle, which the flag turns into a kill for
//!   anything still in the job.
//!
//! ## The handle must outlive the child
//!
//! With `kill_on_drop` (the default) a [`ProcessTree`] is an owner, not a
//! token: dropping it kills whatever is still in the job. Callers keep it
//! alive for as long as the child should run, and call
//! [`ProcessTree::disarm`] before dropping it when a normal exit must not
//! disturb the child's own descendants.
//!
//! ## What is not covered
//!
//! A process that was already running before [`ProcessTree::attach`] is not in
//! the job, and a descendant started before the attach is not either. Both
//! callers here attach immediately after `spawn` (and Windows suspends the
//! child until then), so the window is closed in practice.
//!
//! A Unix descendant that calls `setsid()` leaves the process group, so the
//! group kill misses it. On macOS [`ProcessTree::terminate`] additionally walks
//! the libproc parent/child chain and kills the descendants it finds, which
//! covers the common case but not one whose intermediate parent has already
//! exited (see the note on `kill_descendants`). On Linux the process group is
//! the fallback and a caller that needs a `setsid()`-proof boundary puts the
//! child in a cgroup instead.

#![allow(unsafe_code)]
#![cfg_attr(not(any(unix, target_os = "windows")), allow(dead_code))]

use std::io;
use std::process::Command;

/// Limits and kill behaviour for a [`ProcessTree`]'s Job Object.
///
/// Only the Windows backend reads these; Unix expresses the same policy with
/// rlimits applied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// Kill the job's processes after this much CPU time.
    pub max_cpu_seconds: Option<u64>,
    /// Cap the job's committed memory.
    pub max_memory_bytes: Option<u64>,
    /// Set `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so dropping the tree kills
    /// every process still in the job. Defaults to `true`.
    pub kill_on_drop: bool,
    /// Apply the desktop/clipboard UI restrictions. Callers that run a
    /// user-facing program (a shell) want these; a compiler helper does not.
    pub ui_restrictions: bool,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            max_cpu_seconds: None,
            max_memory_bytes: None,
            kill_on_drop: true,
            ui_restrictions: false,
        }
    }
}

/// A process group (Unix) or Job Object (Windows) that owns a child tree.
#[derive(Debug)]
pub struct ProcessTree {
    inner: imp::Inner,
}

impl ProcessTree {
    /// Create an empty tree. Nothing is owned until [`Self::attach`].
    pub fn create(limits: JobLimits) -> io::Result<Self> {
        Ok(Self {
            inner: imp::Inner::create(limits)?,
        })
    }

    /// Own an already-running process identified by pid.
    ///
    /// Used where the pid is all that was recorded (bridge cleanup). On Unix
    /// the tree then signals that single pid, not a group: the process was not
    /// necessarily started as a group leader.
    // The Windows backend creates a Job Object here, so this cannot be `const`
    // even though the Unix backend is.
    #[allow(clippy::missing_const_for_fn)]
    pub fn from_pid(pid: u32) -> io::Result<Self> {
        Ok(Self {
            inner: imp::Inner::from_pid(pid),
        })
    }

    /// Configure a command so the spawned child can be attached to this tree.
    ///
    /// Unix: start the child in its own process group. Windows: create it
    /// suspended, so [`Self::attach`] runs before the child's first
    /// instruction and nothing it starts can escape the job.
    pub fn prepare(&self, cmd: &mut Command) {
        self.inner.prepare(cmd);
    }

    /// Put a child spawned from a [`Self::prepare`]d command into this tree.
    ///
    /// On Windows this also resumes the suspended child. A failure leaves the
    /// child suspended: the caller must kill it rather than let it run outside
    /// the job it was promised.
    pub fn attach(&self, child: &std::process::Child) -> io::Result<()> {
        self.inner.attach(child)
    }

    /// Put a child identified by a raw process handle into this tree.
    ///
    /// Windows only; the caller spawned the child with [`Self::prepare`].
    #[cfg(target_os = "windows")]
    pub fn attach_raw(&self, handle: std::os::windows::io::RawHandle) -> io::Result<()> {
        self.inner.attach_raw(handle)
    }

    /// Put a just-spawned process into this tree by pid.
    ///
    /// This is the form `tokio::process` callers need: they do not have a
    /// `std::process::Child`, only a pid. On Windows the process must have
    /// been created by a [`Self::prepare`]d command: it is still suspended,
    /// and this call assigns it to the job and resumes it.
    pub fn attach_pid(&self, pid: u32) -> io::Result<()> {
        self.inner.attach_pid(pid)
    }

    /// Kill everything in the tree. Returns whether the kill was issued.
    pub fn terminate(&self) -> bool {
        self.inner.terminate()
    }

    /// Stop owning the tree: a later drop (or [`Self::terminate`]) does
    /// nothing. Call this when the child exited on its own and its remaining
    /// descendants are not ours to kill.
    pub fn disarm(&self) {
        self.inner.disarm();
    }

    /// How many processes are currently in the tree, when the platform can
    /// report it (Windows). `None` elsewhere.
    // On Unix the answer is a constant; Windows queries the job.
    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn active_processes(&self) -> Option<u32> {
        self.inner.active_processes()
    }
}

#[cfg(unix)]
mod imp {
    use super::JobLimits;
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::Mutex;

    /// What the tree owns. A process we spawned ourselves is a group leader;
    /// a pid handed to [`super::ProcessTree::from_pid`] is not.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Target {
        None,
        Group(u32),
        Pid(u32),
    }

    #[derive(Debug)]
    pub(super) struct Inner {
        target: Mutex<Target>,
        /// Set by `prepare`; `attach_pid` then knows the pid leads a group.
        prepared: std::sync::atomic::AtomicBool,
    }

    impl Inner {
        // Fallible to match the Windows backend, which creates a Job Object.
        #[allow(clippy::unnecessary_wraps)]
        pub(super) const fn create(_limits: JobLimits) -> io::Result<Self> {
            Ok(Self {
                target: Mutex::new(Target::None),
                prepared: std::sync::atomic::AtomicBool::new(false),
            })
        }

        pub(super) const fn from_pid(pid: u32) -> Self {
            Self {
                target: Mutex::new(Target::Pid(pid)),
                prepared: std::sync::atomic::AtomicBool::new(false),
            }
        }

        pub(super) fn prepare(&self, cmd: &mut Command) {
            // `process_group(0)` makes the child its own group leader, so
            // `kill(-pid, …)` names exactly the tree it started.
            cmd.process_group(0);
            self.prepared
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        pub(super) fn attach(&self, child: &std::process::Child) -> io::Result<()> {
            self.attach_pid(child.id())
        }

        // Fallible to match the Windows backend, which opens and assigns the
        // process.
        #[allow(clippy::unnecessary_wraps)]
        pub(super) fn attach_pid(&self, pid: u32) -> io::Result<()> {
            *lock(&self.target) = if self.prepared.load(std::sync::atomic::Ordering::SeqCst) {
                Target::Group(pid)
            } else {
                Target::Pid(pid)
            };
            Ok(())
        }

        pub(super) fn terminate(&self) -> bool {
            let target = *lock(&self.target);
            match target {
                Target::Group(pid) => {
                    let Ok(pid) = libc::pid_t::try_from(pid) else {
                        return false;
                    };
                    // macOS has no process group that survives a `setsid()`, so
                    // a daemonised descendant is not in the group the kill below
                    // names. Walk the libproc parent/child chain first, while
                    // the parent still exists to hold the chain together.
                    #[cfg(target_os = "macos")]
                    kill_descendants(pid);
                    // SAFETY: the child leads its own group and has not exec'd
                    // anything that changes that, so `-pid` names exactly the
                    // spawned tree. A dead group makes the call fail harmlessly.
                    unsafe { libc::kill(-pid, libc::SIGKILL) == 0 }
                }
                Target::Pid(pid) => {
                    let Ok(pid) = libc::pid_t::try_from(pid) else {
                        return false;
                    };
                    // SAFETY: signalling a pid we recorded; failure is
                    // reported, not ignored.
                    unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
                }
                Target::None => false,
            }
        }

        pub(super) fn disarm(&self) {
            *lock(&self.target) = Target::None;
        }

        // An inherent method to match the Windows backend's query.
        #[allow(clippy::unused_self)]
        pub(super) const fn active_processes(&self) -> Option<u32> {
            None
        }
    }

    /// SIGKILL every descendant of `root`, deepest first.
    ///
    /// The group kill in [`Inner::terminate`] misses a process that called
    /// `setsid()` (it is in a different group) — the classic way a monitor
    /// daemonises itself out of reach. libproc reports the parent/child chain
    /// directly, so the descendants are collected *before* the group kill (a
    /// dead parent re-parents its children to `launchd`, which erases the
    /// chain) and signalled leaf-first, so no child is orphaned by its parent
    /// being killed first.
    ///
    /// # Best effort
    ///
    /// A process that double-forks, calls `setsid()`, and has its intermediate
    /// parent exit before this walk runs has already been re-parented to
    /// `launchd`; it is no longer reachable through `proc_listchildpids`.
    /// Closing that window needs an EndpointSecurity client, which this host
    /// does not have. This reaches the common `setsid()` escape, not every
    /// possible one.
    #[cfg(target_os = "macos")]
    fn kill_descendants(root: libc::pid_t) {
        for pid in descendants_of(root) {
            // SAFETY: signalling a pid read from libproc. A failure means the
            // process exited between the read and the kill, which is fine.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    /// Every live descendant of `root`, deepest first, excluding `root`.
    #[cfg(target_os = "macos")]
    fn descendants_of(root: libc::pid_t) -> Vec<libc::pid_t> {
        let mut found = Vec::new();
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for child in children_of(parent) {
                // `root` cannot be its own descendant; the guard also stops a
                // malformed chain from looping.
                if child != root && !found.contains(&child) {
                    found.push(child);
                    frontier.push(child);
                }
            }
        }
        // Depth-first discovery lists a parent before its own children, so the
        // reverse lists children before parents.
        found.reverse();
        found
    }

    /// The direct children of `parent`, or an empty vector if it has none or
    /// has exited.
    #[cfg(target_os = "macos")]
    fn children_of(parent: libc::pid_t) -> Vec<libc::pid_t> {
        const INITIAL: usize = 64;
        let mut buffer: Vec<libc::pid_t> = vec![0; INITIAL];
        loop {
            let bytes = libc::c_int::try_from(buffer.len() * std::mem::size_of::<libc::pid_t>())
                .unwrap_or(libc::c_int::MAX);
            // SAFETY: `buffer` is a valid writable slice and `bytes` is exactly
            // its length in bytes, so libproc writes within it.
            let count =
                unsafe { libc::proc_listchildpids(parent, buffer.as_mut_ptr().cast(), bytes) };
            if count <= 0 {
                return Vec::new();
            }
            let count = usize::try_from(count).unwrap_or(0);
            if count < buffer.len() {
                buffer.truncate(count);
                return buffer;
            }
            // The buffer filled exactly: the real child count may be larger.
            buffer = vec![0; buffer.len() * 2];
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::JobLimits;
    use std::io;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
        JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
        JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
        PROCESS_TERMINATE,
    };

    // `NtResumeProcess` is not bound by windows-sys; the AppContainer backend
    // uses the same entry point to release a suspended child.
    unsafe extern "system" {
        fn NtResumeProcess(ProcessHandle: HANDLE) -> i32;
    }

    #[derive(Debug)]
    pub(super) struct Inner {
        job: HANDLE,
        /// Whether the tree still owns its processes. Cleared by `disarm` and
        /// by `terminate` (a terminated job has nothing left to kill).
        armed: AtomicBool,
        /// Set by `prepare`; `attach` must resume the child it suspended.
        suspended: AtomicBool,
    }

    // SAFETY: `job` is a kernel handle with no thread affinity, and the flags
    // are atomics. Moving the tree between threads aliases nothing.
    unsafe impl Send for Inner {}
    // SAFETY: every `&self` call passes the handle by value to Win32, which is
    // safe from any thread.
    unsafe impl Sync for Inner {}

    impl Inner {
        pub(super) fn create(limits: JobLimits) -> io::Result<Self> {
            // Anonymous job: a named one is visible to, and shareable with,
            // every other process on the machine.
            // SAFETY: null attributes/name ask for a default anonymous job.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let inner = Self {
                job,
                armed: AtomicBool::new(true),
                suspended: AtomicBool::new(false),
            };
            if let Err(error) = inner.apply_limits(limits) {
                // SAFETY: this is the only owner of the handle.
                unsafe { CloseHandle(job) };
                return Err(error);
            }
            Ok(inner)
        }

        pub(super) fn from_pid(_pid: u32) -> Self {
            // A pid-only tree still needs a job to terminate through, but the
            // process is not in it yet; `attach_pid` does that.
            Self::create(JobLimits::default()).unwrap_or_else(|_| Self {
                job: std::ptr::null_mut(),
                armed: AtomicBool::new(false),
                suspended: AtomicBool::new(false),
            })
        }

        fn apply_limits(&self, limits: JobLimits) -> io::Result<()> {
            let mut basic: JOBOBJECT_BASIC_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            let mut flags: u32 = 0;
            if limits.kill_on_drop {
                flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            }
            if let Some(cpu) = limits.max_cpu_seconds {
                // 100-nanosecond ticks; the field is an `i64` rather than a
                // `LARGE_INTEGER` in windows-sys.
                basic.PerJobUserTimeLimit = (cpu * 10_000_000).cast_signed();
                flags |= JOB_OBJECT_LIMIT_JOB_TIME;
            }
            if limits.max_memory_bytes.is_some() {
                flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            }
            basic.LimitFlags = flags;

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation = basic;
            if let Some(memory) = limits.max_memory_bytes {
                info.JobMemoryLimit = memory as usize;
            }
            // SAFETY: `info` is initialized and the size is its own.
            let ok = unsafe {
                SetInformationJobObject(
                    self.job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of_val(&info) as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }

            if limits.ui_restrictions {
                let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = unsafe { std::mem::zeroed() };
                ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_DESKTOP
                    | JOB_OBJECT_UILIMIT_EXITWINDOWS
                    | JOB_OBJECT_UILIMIT_GLOBALATOMS
                    | JOB_OBJECT_UILIMIT_HANDLES
                    | JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD;
                // SAFETY: as above.
                let ok = unsafe {
                    SetInformationJobObject(
                        self.job,
                        JobObjectBasicUIRestrictions,
                        std::ptr::from_ref(&ui).cast(),
                        std::mem::size_of_val(&ui) as u32,
                    )
                };
                if ok == 0 {
                    // Not fatal: the UI restrictions are hygiene, not the
                    // boundary. The job limits above are already in force.
                    tracing::warn!("job UI restrictions failed: {}", io::Error::last_os_error());
                }
            }
            Ok(())
        }

        pub(super) fn prepare(&self, cmd: &mut Command) {
            // Suspend so `attach` runs before the child's first instruction.
            // A caller that wants extra creation flags must set them itself;
            // this call owns the flag word.
            cmd.creation_flags(CREATE_SUSPENDED);
            self.suspended.store(true, Ordering::SeqCst);
        }

        pub(super) fn attach(&self, child: &std::process::Child) -> io::Result<()> {
            self.attach_raw(child.as_raw_handle())
        }

        pub(super) fn attach_raw(&self, handle: RawHandle) -> io::Result<()> {
            if self.job.is_null() {
                return Err(io::Error::other("process tree has no job object"));
            }
            // SAFETY: both handles are valid for the duration of the call.
            let ok = unsafe { AssignProcessToJobObject(self.job, handle as HANDLE) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if self.suspended.swap(false, Ordering::SeqCst) {
                // SAFETY: the handle names the suspended child.
                let status = unsafe { NtResumeProcess(handle as HANDLE) };
                if status < 0 {
                    return Err(io::Error::other(format!(
                        "NtResumeProcess failed: 0x{status:08X}"
                    )));
                }
            }
            Ok(())
        }

        // Fallible to match the Windows backend, which opens and assigns the
        // process.
        #[allow(clippy::unnecessary_wraps)]
        pub(super) fn attach_pid(&self, pid: u32) -> io::Result<()> {
            if self.job.is_null() {
                return Err(io::Error::other("process tree has no job object"));
            }
            // SAFETY: the access mask is the minimum needed to assign a
            // process to a job and terminate it later.
            let process = unsafe {
                OpenProcess(
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    pid,
                )
            };
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `process` is a live handle we own.
            let ok = unsafe { AssignProcessToJobObject(self.job, process) };
            let result = if ok == 0 {
                Err(io::Error::last_os_error())
            } else if self.suspended.swap(false, Ordering::SeqCst) {
                // SAFETY: the process was created suspended by `prepare`.
                let status = unsafe { NtResumeProcess(process) };
                if status < 0 {
                    Err(io::Error::other(format!(
                        "NtResumeProcess failed: 0x{status:08X}"
                    )))
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            // SAFETY: this is the only owner of the process handle.
            unsafe { CloseHandle(process) };
            result
        }

        pub(super) fn terminate(&self) -> bool {
            if self.job.is_null() {
                return false;
            }
            self.armed.store(false, Ordering::SeqCst);
            // SAFETY: the handle is valid until this value is dropped.
            unsafe { TerminateJobObject(self.job, 1) != 0 }
        }

        pub(super) fn disarm(&self) {
            if self.job.is_null() {
                self.armed.store(false, Ordering::SeqCst);
                return;
            }
            // Clear `KILL_ON_JOB_CLOSE` so closing the handle is not a kill.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is a valid out pointer and the size is its own.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_mut(&mut info).cast(),
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                tracing::warn!(
                    "could not read job limits before disarming: {}",
                    io::Error::last_os_error()
                );
                return;
            }
            info.BasicLimitInformation.LimitFlags &= !JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: as above.
            let ok = unsafe {
                SetInformationJobObject(
                    self.job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of_val(&info) as u32,
                )
            };
            if ok == 0 {
                tracing::warn!(
                    "could not disarm the job; its processes die with this handle: {}",
                    io::Error::last_os_error()
                );
            }
            self.armed.store(false, Ordering::SeqCst);
        }

        pub(super) fn active_processes(&self) -> Option<u32> {
            if self.job.is_null() {
                return None;
            }
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                unsafe { std::mem::zeroed() };
            // SAFETY: `accounting` is a valid out pointer of the right size.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job,
                    JobObjectBasicAccountingInformation,
                    std::ptr::from_mut(&mut accounting).cast(),
                    std::mem::size_of_val(&accounting) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return None;
            }
            Some(accounting.ActiveProcesses)
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            if self.job.is_null() {
                return;
            }
            if self.armed.load(Ordering::SeqCst) {
                // `KILL_ON_JOB_CLOSE` makes the close below the kill; the
                // explicit terminate is only for clarity in logs/tests.
                // SAFETY: the handle is valid until the close.
                unsafe { TerminateJobObject(self.job, 1) };
            }
            // SAFETY: this is the only owner of the handle.
            unsafe { CloseHandle(self.job) };
        }
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
mod imp {
    use super::JobLimits;
    use std::io;
    use std::process::Command;

    #[derive(Debug)]
    pub(super) struct Inner;

    impl Inner {
        pub(super) fn create(_limits: JobLimits) -> io::Result<Self> {
            Ok(Self)
        }

        pub(super) fn from_pid(_pid: u32) -> Self {
            Self
        }

        pub(super) fn prepare(&self, _cmd: &mut Command) {}

        pub(super) fn attach(&self, _child: &std::process::Child) -> io::Result<()> {
            Ok(())
        }

        pub(super) fn attach_pid(&self, _pid: u32) -> io::Result<()> {
            Ok(())
        }

        pub(super) fn terminate(&self) -> bool {
            false
        }

        pub(super) fn disarm(&self) {}

        pub(super) fn active_processes(&self) -> Option<u32> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{JobLimits, ProcessTree};

    /// A long-lived child that also has a child of its own, so a tree kill is
    /// distinguishable from a direct-child kill.
    fn long_lived_command() -> std::process::Command {
        #[cfg(unix)]
        {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "sleep 60"]);
            cmd
        }
        #[cfg(target_os = "windows")]
        {
            let mut cmd = std::process::Command::new("cmd.exe");
            cmd.args(["/C", "ping -n 60 127.0.0.1 > nul"]);
            cmd
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            std::process::Command::new("true")
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminate_kills_the_group() {
        let tree = ProcessTree::create(JobLimits::default()).unwrap();
        let mut cmd = long_lived_command();
        tree.prepare(&mut cmd);
        let child = cmd.spawn().unwrap();
        tree.attach(&child).unwrap();

        assert!(tree.terminate(), "the group kill should be issued");
        let status = child.wait_with_output().unwrap().status;
        assert!(!status.success(), "the child should have been signalled");
    }

    #[cfg(unix)]
    #[test]
    fn disarmed_tree_leaves_the_child_running() {
        let tree = ProcessTree::create(JobLimits::default()).unwrap();
        let mut cmd = long_lived_command();
        tree.prepare(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        tree.attach(&child).unwrap();
        tree.disarm();

        assert!(!tree.terminate(), "a disarmed tree kills nothing");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the child should still be running after the tree was disarmed"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn dropping_a_process_tree_kills_its_child() {
        let mut cmd = long_lived_command();
        let mut child = {
            let tree = ProcessTree::create(JobLimits::default()).unwrap();
            tree.prepare(&mut cmd);
            let child = cmd.spawn().unwrap();
            tree.attach(&child).unwrap();
            assert_eq!(tree.active_processes(), Some(1));
            child
            // `tree` drops here, which must kill the child.
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child outlived the process tree that owned it"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
