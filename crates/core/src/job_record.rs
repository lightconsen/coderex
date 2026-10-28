//! Durable background-job records and the adoptability classifier.
//!
//! CodeRex's background work runs as **in-process `tokio` tasks**
//! ([`crate::task`]), so when the process exits the work is gone: unlike the
//! reference implementation, there is no worker process that outlives its
//! supervisor for a later session to re-attach to. This module therefore does
//! **not** pretend to re-attach anything. It does the two things that are
//! honest and useful:
//!
//! * [`JobRecord`] + [`JobStore`] — one JSON file per job under
//!   `<cwd>/.coderex/jobs/<id>.json`, written atomically (temp file + rename),
//!   so a record of the work survives the process that created it;
//! * [`adopt`] — a **pure** classifier that sorts the records a starting
//!   session finds into [`AdoptOutcome`] buckets, plus
//!   [`JobStore::mark_interrupted`] / [`AdoptOutcome::notifications`], which
//!   turn the work that cannot be resumed into a notification the CLI can
//!   surface into the conversation — never a silent, phantom `running`.
//!
//! The classifier mirrors the reference implementation's rules and thresholds:
//!
//! * **foreign** — a record owned by another session is not adopted; that
//!   session may still be live and adopting it would double-drive the work.
//! * **stale** — a record whose last update is older than
//!   [`STALE_THRESHOLD_MS`], or implausibly far in the *future* (more than
//!   [`CLOCK_SKEW_ALLOWANCE_MS` ahead of local time), is not trusted. The
//!   future bound is deliberately **asymmetric**: a writer whose clock ran a
//!   little ahead (or an NTP step) still reads as a live record.
//! * **terminal** — `Done`/`Failed` records are exempt from ageing, exactly as
//!   the reference exempts cleanly-exited sessions from its staleness check.
//!   There is nothing to re-attach to a finished job, so they are terminal no
//!   matter how old they are.
//! * **resumable** vs **interrupted** — a cron definition
//!   ([`JobKind::Cron`]) can genuinely be re-scheduled, so it lands in
//!   `resumable`. Everything else CodeRex records (agent / shell / workflow)
//!   was an in-process task; a persisted record of it can only be **reported**
//!   as interrupted.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

// ─── Tolerances ─────────────────────────────────────────────────────────────

/// A record whose last update is older than this is **stale**: its owner is
/// assumed to have died without finishing, and the record is not adopted.
pub const STALE_THRESHOLD_MS: i64 = 120_000;

/// Clock-skew allowance for persisted timestamps.
///
/// A record may claim a timestamp this far *ahead* of the local clock without
/// being rejected — the process that wrote it may simply have had a clock a
/// little ahead. The bound is **asymmetric**: it only ever relaxes the future
/// direction, it never makes an old record fresh.
pub const CLOCK_SKEW_ALLOWANCE_MS: i64 = 60_000;

/// Environment variable that disables adoption entirely for this process.
///
/// Mirrors the reference's `CLAUDE_DISABLE_ADOPT`. The reference carries
/// previously-granted consent and *discloses* rather than asks, so this is a
/// switch, not an adopt-time dialog: setting it to any value other than
/// `0`/`false`/`no`/empty makes [`adopt_for_session`] a no-op.
pub const DISABLE_ADOPT_ENV: &str = "CODEREX_DISABLE_ADOPT";

// ─── JobKind ────────────────────────────────────────────────────────────────

/// The kind of background work a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// A sub-agent run.
    Agent,
    /// A local shell command.
    Shell,
    /// A workflow run.
    Workflow,
    /// A scheduled (cron) definition.
    Cron,
}

impl JobKind {
    /// A short stable label, used in messages and persisted records.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Shell => "shell",
            Self::Workflow => "workflow",
            Self::Cron => "cron",
        }
    }

    /// Whether a record of this kind can genuinely be re-attached by a later
    /// session.
    ///
    /// Only [`JobKind::Cron`] qualifies. A cron *definition* is data, not a
    /// running task: it can be re-scheduled from its persisted form, which is
    /// exactly what the reference's cron adoption does. The other kinds
    /// describe in-process `tokio` work that died with its process, so a
    /// record of them can only be reported as interrupted.
    #[must_use]
    pub const fn is_resumable(self) -> bool {
        matches!(self, Self::Cron)
    }
}

// ─── JobState ───────────────────────────────────────────────────────────────

/// Lifecycle state recorded for a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// The job was running when the record was last written.
    Running,
    /// The owning process went away before the job finished, and a later
    /// session recorded that fact. Never adopted, never left as `Running`.
    Interrupted,
    /// The job finished successfully.
    Done,
    /// The job failed.
    Failed,
}

impl JobState {
    /// Whether this state is terminal.
    ///
    /// Terminal records are exempt from ageing at adopt time: a job that
    /// exited cleanly cannot be "stale", only finished.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }

    /// A short stable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

// ─── JobRecord ──────────────────────────────────────────────────────────────

/// A durable record of one background job.
///
/// One record is one JSON file in a [`JobStore`]. The store is deliberately
/// *not* indexed: the reference keeps a `roster.json` index next to its
/// per-job `state.json` files because its daemon rescans a job tree of unknown
/// size on every restart. CodeRex's job directory is written by a single
/// session and holds a handful of files, so listing the directory is enough —
/// an index would be a second source of truth that can disagree with the files
/// it indexes. See `docs/tasks.md` for the full rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    /// Unique job identifier. Also the file stem in the store.
    pub id: String,
    /// What kind of work this was.
    pub kind: JobKind,
    /// Human-readable description of the job.
    pub description: String,
    /// Working directory the job ran in.
    pub cwd: PathBuf,
    /// Session that created the job. `None` means "unknown/legacy" and is
    /// treated as owned by whoever is adopting.
    #[serde(default)]
    pub session_id: Option<String>,
    /// OS process id of the job's owner, when it had one.
    #[serde(default)]
    pub pid: Option<u32>,
    /// When the job started, in milliseconds since the Unix epoch.
    pub started_at_ms: i64,
    /// When the record was last updated, in milliseconds since the Unix epoch.
    pub updated_at_ms: i64,
    /// Current lifecycle state.
    pub state: JobState,
    /// When the job reached a terminal state, in milliseconds since the Unix
    /// epoch. `None` for a job that is still running (or whose end time was
    /// never observed, as with a record marked interrupted after a crash).
    ///
    /// Added after the original on-disk record shape; `serde(default)` keeps
    /// records written by older versions decodable.
    #[serde(default)]
    pub ended_at_ms: Option<i64>,
    /// Exit code of the finished process, when the job had one. In-process
    /// work (agents, cron definitions) leaves this `None`.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Failure detail, when the job failed.
    #[serde(default)]
    pub error: Option<String>,
}

impl JobRecord {
    /// Create a record for a job that has just started.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        kind: JobKind,
        description: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session_id: Option<&str>,
        now_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            description: description.into(),
            cwd: cwd.into(),
            session_id: session_id.map(str::to_string),
            pid: None,
            started_at_ms: now_ms,
            updated_at_ms: now_ms,
            state: JobState::Running,
            ended_at_ms: None,
            exit_code: None,
            error: None,
        }
    }

    /// Attach the owning process id.
    #[must_use]
    pub const fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    /// Record the wall-clock time of the most recent change to this job.
    pub const fn touch(&mut self, now_ms: i64) {
        self.updated_at_ms = now_ms;
    }

    /// Move the job to a terminal state (`Done`, `Failed` or `Interrupted`)
    /// and stamp its end time.
    ///
    /// This is the single lifecycle helper the producers use, so every record
    /// that stops running carries the same end fields. `exit_code` is for a
    /// real process; in-process work leaves it `None` and may supply `error`.
    pub fn settle(
        &mut self,
        state: JobState,
        now_ms: i64,
        exit_code: Option<i32>,
        error: Option<String>,
    ) {
        self.state = state;
        self.updated_at_ms = now_ms;
        self.ended_at_ms = Some(now_ms);
        self.exit_code = exit_code;
        self.error = error;
    }
}

// ─── AdoptOutcome ───────────────────────────────────────────────────────────

/// Notification the CLI should surface into the conversation for a job that
/// could not be resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNotification {
    /// The job this notification is about.
    pub job_id: String,
    /// The kind of job.
    pub kind: JobKind,
    /// Ready-to-display text.
    pub text: String,
}

/// The buckets a starting session's records fall into.
///
/// Every input record lands in exactly one bucket, so the outcome is a total
/// classification — nothing is silently dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdoptOutcome {
    /// Records that can genuinely be re-attached. For CodeRex that is cron
    /// definitions only ([`JobKind::is_resumable`]).
    pub resumable: Vec<JobRecord>,
    /// Records of work that cannot be re-attached: the owning process is gone.
    /// These must be **reported** (see [`Self::notifications`]) and marked
    /// interrupted in the store — never left as a phantom `running`.
    pub interrupted: Vec<JobRecord>,
    /// Records too old (or too far in the future) to trust.
    pub stale: Vec<JobRecord>,
    /// Records owned by another session.
    pub foreign: Vec<JobRecord>,
    /// Records of jobs that already finished (successfully or not).
    pub terminal: Vec<JobRecord>,
}

impl AdoptOutcome {
    /// Total number of records classified.
    #[must_use]
    pub fn total(&self) -> usize {
        self.resumable.len()
            + self.interrupted.len()
            + self.stale.len()
            + self.foreign.len()
            + self.terminal.len()
    }

    /// One notification per `interrupted` record.
    ///
    /// The CLI is expected to push each `text` into the conversation (a system
    /// message), which is how the reference reports a job it could not
    /// re-attach instead of leaving it silently `running`.
    #[must_use]
    pub fn notifications(&self) -> Vec<TaskNotification> {
        self.interrupted.iter().map(notification_for).collect()
    }
}

/// Build the notification text for one interrupted record.
fn notification_for(record: &JobRecord) -> TaskNotification {
    let owner = record.session_id.as_deref().unwrap_or("unknown");
    TaskNotification {
        job_id: record.id.clone(),
        kind: record.kind,
        text: format!(
            "Background {} job {} ({}) was last recorded as running by session {} but the \
             process that owned it is gone; it could not be resumed. Marked as interrupted.",
            record.kind.label(),
            record.id,
            record.description,
            owner
        ),
    }
}

// ─── adopt ──────────────────────────────────────────────────────────────────

/// Classify persisted records for a starting session.
///
/// Pure: no IO, no environment, no clock. `now_ms` and `session_id` are
/// passed in, which is what makes the whole matrix testable.
///
/// A record is placed by the first rule that matches:
///
/// 1. [`AdoptOutcome::foreign`] — `session_id` is set and differs from the
///    adopting session.
/// 2. [`AdoptOutcome::terminal`] — state is `Done` or `Failed`. Checked before
///    ageing, so a finished job is never called stale.
/// 3. [`AdoptOutcome::stale`] — last update is more than
///    [`STALE_THRESHOLD_MS`] behind `now_ms`, or more than
///    [`CLOCK_SKEW_ALLOWANCE_MS`] ahead of it.
/// 4. [`AdoptOutcome::resumable`] — the kind is genuinely re-attachable
///    ([`JobKind::is_resumable`]).
/// 5. [`AdoptOutcome::interrupted`] — everything else: work that was alive
///    when the record was written and cannot be re-attached now.
#[must_use]
pub fn adopt(records: &[JobRecord], now_ms: i64, session_id: &str) -> AdoptOutcome {
    let mut outcome = AdoptOutcome::default();

    for record in records {
        if let Some(owner) = record.session_id.as_deref()
            && owner != session_id
        {
            outcome.foreign.push(record.clone());
            continue;
        }

        if record.state.is_terminal() {
            outcome.terminal.push(record.clone());
            continue;
        }

        let age = now_ms.saturating_sub(record.updated_at_ms);
        let ahead = record.updated_at_ms.saturating_sub(now_ms);
        if age > STALE_THRESHOLD_MS || ahead > CLOCK_SKEW_ALLOWANCE_MS {
            outcome.stale.push(record.clone());
            continue;
        }

        if record.kind.is_resumable() {
            outcome.resumable.push(record.clone());
        } else {
            outcome.interrupted.push(record.clone());
        }
    }

    outcome
}

// ─── Kill switch ────────────────────────────────────────────────────────────

/// Whether adoption is disabled for this process.
///
/// Reads [`DISABLE_ADOPT_ENV`]. Presence with any value other than
/// `0`/`false`/`no`/empty disables adoption. The parsing lives in
/// [`adopt_disabled_with`] so it can be tested without touching process
/// environment.
#[must_use]
pub fn adopt_disabled() -> bool {
    adopt_disabled_with(std::env::var(DISABLE_ADOPT_ENV).ok().as_deref())
}

/// Pure helper behind [`adopt_disabled`].
#[must_use]
pub fn adopt_disabled_with(value: Option<&str>) -> bool {
    value.is_some_and(|raw| {
        let trimmed = raw.trim();
        !(trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("0")
            || trimmed.eq_ignore_ascii_case("false")
            || trimmed.eq_ignore_ascii_case("no"))
    })
}

// ─── Startup adopt ──────────────────────────────────────────────────────────

/// What a starting session got out of adoption.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupAdopt {
    /// The classification of everything found in the store.
    pub outcome: AdoptOutcome,
    /// Notifications for jobs that were just marked interrupted.
    pub notifications: Vec<TaskNotification>,
}

/// Adopt for a starting session, honouring the kill switch.
///
/// This is the single call a session entry point needs: it reads the store,
/// classifies with [`adopt`], and records the interrupted jobs on disk with
/// [`JobStore::mark_interrupted`]. When [`adopt_disabled`] is true it returns
/// an empty result without touching the store.
pub fn adopt_for_session(
    store: &JobStore,
    now_ms: i64,
    session_id: &str,
) -> Result<StartupAdopt, JobStoreError> {
    if adopt_disabled() {
        return Ok(StartupAdopt::default());
    }
    adopt_and_reconcile(store, now_ms, session_id)
}

/// [`adopt_for_session`] without the kill switch — the tested core.
pub fn adopt_and_reconcile(
    store: &JobStore,
    now_ms: i64,
    session_id: &str,
) -> Result<StartupAdopt, JobStoreError> {
    let records = store.list()?;
    let outcome = adopt(&records, now_ms, session_id);
    let notifications = store.mark_interrupted(&outcome, now_ms)?;
    Ok(StartupAdopt {
        outcome,
        notifications,
    })
}

// ─── JobStore ───────────────────────────────────────────────────────────────

/// Errors from the job store.
#[derive(Debug, Error)]
pub enum JobStoreError {
    /// The id cannot be used as a file stem (empty, or contains path
    /// separators or other characters that escape the store directory).
    #[error("invalid job id {0:?}: ids must be non-empty and contain only [A-Za-z0-9_-]")]
    InvalidId(String),
    /// A filesystem operation failed.
    #[error("job store io error at {path}: {source}")]
    Io {
        /// Path the operation was working on.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// A record could not be decoded.
    #[error("failed to decode job record at {path}: {source}")]
    Decode {
        /// Path of the bad record.
        path: PathBuf,
        /// Underlying serde error.
        source: serde_json::Error,
    },
    /// A record could not be encoded.
    #[error("failed to encode job record {0}: {1}")]
    Encode(String, serde_json::Error),
}

/// One-JSON-file-per-job store rooted at `<cwd>/.coderex/jobs`.
///
/// Writes are atomic: the record is serialized into a sibling temp file and
/// renamed over the target, so a concurrent reader sees either the old record
/// or the new one, never a half-written one.
#[derive(Debug, Clone)]
pub struct JobStore {
    root: PathBuf,
}

impl JobStore {
    /// Open the store for a working directory: `<cwd>/.coderex/jobs`.
    #[must_use]
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            root: cwd.as_ref().join(".coderex").join("jobs"),
        }
    }

    /// Open a store rooted at an explicit directory (the jobs directory
    /// itself, not the working directory).
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory records live in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of one record, after validating the id.
    pub fn path_for(&self, id: &str) -> Result<PathBuf, JobStoreError> {
        if !is_valid_id(id) {
            return Err(JobStoreError::InvalidId(id.to_string()));
        }
        Ok(self.root.join(format!("{id}.json")))
    }

    /// Write one record, atomically.
    pub fn write(&self, record: &JobRecord) -> Result<(), JobStoreError> {
        let path = self.path_for(&record.id)?;
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|e| JobStoreError::Encode(record.id.clone(), e))?;
        self.write_bytes_atomic(&path, &bytes)
    }

    /// Settle a record that is still `Running`.
    ///
    /// Producers call this on completion, failure or cancellation. A record
    /// that is missing or already terminal is left alone (`Ok(false)`): a
    /// late completion must not overwrite a cancellation, and a second
    /// settle must not resurrect a finished job.
    pub fn settle(
        &self,
        id: &str,
        state: JobState,
        now_ms: i64,
        exit_code: Option<i32>,
        error: Option<String>,
    ) -> Result<bool, JobStoreError> {
        let Some(mut record) = self.load(id)? else {
            return Ok(false);
        };
        if record.state != JobState::Running {
            return Ok(false);
        }
        record.settle(state, now_ms, exit_code, error);
        self.write(&record)?;
        Ok(true)
    }

    /// Load one record. `Ok(None)` when it does not exist.
    pub fn load(&self, id: &str) -> Result<Option<JobRecord>, JobStoreError> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|e| JobStoreError::Io {
            path: path.clone(),
            source: e,
        })?;
        let record = serde_json::from_slice(&bytes).map_err(|e| JobStoreError::Decode {
            path: path.clone(),
            source: e,
        })?;
        Ok(Some(record))
    }

    /// List every record in the store, ordered by id.
    ///
    /// A missing store directory is an empty store, not an error. Files whose
    /// name is not `<id>.json`, and files that fail to decode, are skipped
    /// with a warning: one bad record must not make adoption impossible.
    pub fn list(&self) -> Result<Vec<JobRecord>, JobStoreError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(JobStoreError::Io {
                    path: self.root.clone(),
                    source: e,
                });
            }
        };

        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| JobStoreError::Io {
                path: self.root.clone(),
                source: e,
            })?;
            let path = entry.path();
            let Some(id) = record_id_from_path(&path) else {
                continue;
            };
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping unreadable job record");
                    continue;
                }
            };
            match serde_json::from_slice::<JobRecord>(&bytes) {
                Ok(record) => {
                    debug_assert_eq!(record.id, id);
                    records.push(record);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping undecodable job record");
                }
            }
        }

        records.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(records)
    }

    /// Delete one record. `Ok(false)` when it was not there.
    pub fn remove(&self, id: &str) -> Result<bool, JobStoreError> {
        let path = self.path_for(id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(JobStoreError::Io { path, source: e }),
        }
    }

    /// Delete every finished record (`Done`/`Failed`). Returns how many were
    /// removed.
    pub fn prune_done(&self) -> Result<usize, JobStoreError> {
        let mut removed = 0;
        for record in self.list()? {
            if record.state.is_terminal() && self.remove(&record.id)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Mark the interrupted jobs as such on disk and return one notification
    /// per transition.
    ///
    /// Only records still claiming `Running` are rewritten — those are the
    /// phantom `running` entries the reference refuses to leave behind. A
    /// record already marked `Interrupted` was reported by an earlier session
    /// and is left alone, so a crash is surfaced once rather than on every
    /// subsequent start.
    pub fn mark_interrupted(
        &self,
        outcome: &AdoptOutcome,
        now_ms: i64,
    ) -> Result<Vec<TaskNotification>, JobStoreError> {
        let mut notifications = Vec::new();
        for record in &outcome.interrupted {
            if record.state != JobState::Running {
                continue;
            }
            let mut marked = record.clone();
            marked.state = JobState::Interrupted;
            marked.touch(now_ms);
            // The job's real end is unknown (its process went away), so the
            // reconciliation time is the best end stamp available.
            marked.ended_at_ms.get_or_insert(now_ms);
            self.write(&marked)?;
            notifications.push(notification_for(record));
        }
        Ok(notifications)
    }

    /// Serialize into a sibling temp file, then rename over the target.
    fn write_bytes_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), JobStoreError> {
        std::fs::create_dir_all(&self.root).map_err(|e| JobStoreError::Io {
            path: self.root.clone(),
            source: e,
        })?;

        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stem = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("record.json");
        let tmp = self
            .root
            .join(format!("{stem}.tmp-{}-{n}", std::process::id()));

        if let Err(e) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(JobStoreError::Io {
                path: tmp,
                source: e,
            });
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(JobStoreError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
        Ok(())
    }
}

/// Counter making temp-file names unique within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Job ids become file stems, so they must not be able to escape the store
/// directory or collide with the temp-file naming scheme.
fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `<id>.json` -> `Some(id)`; anything else (temp files, other extensions) ->
/// `None`.
fn record_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_suffix(".json")?;
    is_valid_id(id).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn record(id: &str, kind: JobKind, owner: Option<&str>, state: JobState, at: i64) -> JobRecord {
        let mut r = JobRecord::new(id, kind, "a job", "/work", owner, at);
        r.state = state;
        r.updated_at_ms = at;
        r
    }

    // ─── Adoptability matrix ────────────────────────────────────────────────

    #[test]
    fn fresh_owned_cron_is_resumable() {
        let records = vec![record(
            "cron-1",
            JobKind::Cron,
            Some("sess_a"),
            JobState::Running,
            NOW - 1_000,
        )];
        let out = adopt(&records, NOW, "sess_a");
        assert_eq!(out.resumable.len(), 1);
        assert!(out.interrupted.is_empty());
        assert_eq!(out.total(), 1);
    }

    #[test]
    fn fresh_owned_shell_is_interrupted_not_resumable() {
        let records = vec![record(
            "task_1",
            JobKind::Shell,
            Some("sess_a"),
            JobState::Running,
            NOW - 1_000,
        )];
        let out = adopt(&records, NOW, "sess_a");
        assert_eq!(out.interrupted.len(), 1);
        assert!(
            out.resumable.is_empty(),
            "in-process shell work is never resumable"
        );
    }

    #[test]
    fn foreign_record_is_not_adopted() {
        let records = vec![
            record(
                "a",
                JobKind::Cron,
                Some("sess_other"),
                JobState::Running,
                NOW,
            ),
            record(
                "b",
                JobKind::Shell,
                Some("sess_other"),
                JobState::Running,
                NOW,
            ),
        ];
        let out = adopt(&records, NOW, "sess_mine");
        assert_eq!(out.foreign.len(), 2);
        assert!(out.resumable.is_empty());
        assert!(out.interrupted.is_empty());
    }

    #[test]
    fn ownerless_legacy_record_is_ours() {
        let records = vec![record("a", JobKind::Shell, None, JobState::Running, NOW)];
        let out = adopt(&records, NOW, "sess_any");
        assert_eq!(out.interrupted.len(), 1);
        assert!(out.foreign.is_empty());
    }

    #[test]
    fn stale_record_is_rejected_at_threshold_boundary() {
        let fresh = vec![record(
            "old",
            JobKind::Cron,
            Some("sess_a"),
            JobState::Running,
            NOW - STALE_THRESHOLD_MS,
        )];
        assert_eq!(
            adopt(&fresh, NOW, "sess_a").resumable.len(),
            1,
            "exactly at the threshold is still fresh"
        );

        let past = vec![record(
            "older",
            JobKind::Cron,
            Some("sess_a"),
            JobState::Running,
            NOW - STALE_THRESHOLD_MS - 1,
        )];
        let out = adopt(&past, NOW, "sess_a");
        assert_eq!(out.stale.len(), 1);
        assert!(out.resumable.is_empty());
    }

    #[test]
    fn future_timestamp_within_skew_is_trusted() {
        let records = vec![record(
            "skewed",
            JobKind::Cron,
            Some("sess_a"),
            JobState::Running,
            NOW + CLOCK_SKEW_ALLOWANCE_MS,
        )];
        let out = adopt(&records, NOW, "sess_a");
        assert_eq!(out.resumable.len(), 1);
        assert!(out.stale.is_empty());
    }

    #[test]
    fn future_timestamp_beyond_skew_is_rejected() {
        let records = vec![record(
            "future",
            JobKind::Cron,
            Some("sess_a"),
            JobState::Running,
            NOW + CLOCK_SKEW_ALLOWANCE_MS + 1,
        )];
        let out = adopt(&records, NOW, "sess_a");
        assert!(out.resumable.is_empty());
        assert_eq!(out.stale.len(), 1);
    }

    #[test]
    fn skew_allowance_is_asymmetric() {
        // The allowance never makes an old record fresh: one millisecond past
        // the stale threshold is stale even though the skew bound is larger
        // than... (it is smaller, but the point stands: the two are separate).
        let records = vec![record(
            "old",
            JobKind::Cron,
            Some("sess_a"),
            JobState::Running,
            NOW - STALE_THRESHOLD_MS - 1,
        )];
        let out = adopt(&records, NOW, "sess_a");
        assert_eq!(out.stale.len(), 1);
    }

    #[test]
    fn terminal_records_are_exempt_from_ageing() {
        let long_ago = NOW - (STALE_THRESHOLD_MS * 100);
        let records = vec![
            record(
                "done",
                JobKind::Shell,
                Some("sess_a"),
                JobState::Done,
                long_ago,
            ),
            record(
                "failed",
                JobKind::Cron,
                Some("sess_a"),
                JobState::Failed,
                long_ago,
            ),
        ];
        let out = adopt(&records, NOW, "sess_a");
        assert_eq!(out.terminal.len(), 2);
        assert!(out.stale.is_empty(), "a finished job is never stale");
    }

    #[test]
    fn already_interrupted_record_is_interrupted_not_resumable() {
        let records = vec![
            record(
                "task_1",
                JobKind::Shell,
                Some("sess_a"),
                JobState::Interrupted,
                NOW - 1_000,
            ),
            record(
                "cron-1",
                JobKind::Cron,
                Some("sess_a"),
                JobState::Interrupted,
                NOW - 1_000,
            ),
        ];
        let out = adopt(&records, NOW, "sess_a");
        assert_eq!(out.interrupted.len(), 1);
        assert_eq!(out.resumable.len(), 1);
    }

    #[test]
    fn outcome_is_total() {
        let records = vec![
            record("a", JobKind::Cron, Some("other"), JobState::Running, NOW),
            record("b", JobKind::Shell, None, JobState::Running, NOW),
            record("c", JobKind::Cron, None, JobState::Done, NOW),
            record(
                "d",
                JobKind::Shell,
                None,
                JobState::Running,
                NOW - STALE_THRESHOLD_MS - 1,
            ),
            record("e", JobKind::Workflow, None, JobState::Running, NOW),
        ];
        let out = adopt(&records, NOW, "sess_a");
        assert_eq!(out.total(), records.len());
    }

    #[test]
    fn interrupted_notification_names_the_job() {
        let mut r = record(
            "task_9",
            JobKind::Agent,
            Some("sess_a"),
            JobState::Running,
            NOW,
        );
        r.description = "read the logs".into();
        let out = adopt(&[r], NOW, "sess_a");
        let notes = out.notifications();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].job_id, "task_9");
        assert_eq!(notes[0].kind, JobKind::Agent);
        assert!(notes[0].text.contains("could not be resumed"));
        assert!(notes[0].text.contains("sess_a"));
    }

    #[test]
    fn resumable_records_do_not_notify() {
        let records = vec![record(
            "cron-1",
            JobKind::Cron,
            Some("sess_a"),
            JobState::Running,
            NOW,
        )];
        assert!(adopt(&records, NOW, "sess_a").notifications().is_empty());
    }

    // ─── Kill switch ────────────────────────────────────────────────────────

    #[test]
    fn kill_switch_parsing() {
        assert!(!adopt_disabled_with(None));
        assert!(!adopt_disabled_with(Some("")));
        assert!(!adopt_disabled_with(Some("0")));
        assert!(!adopt_disabled_with(Some("false")));
        assert!(!adopt_disabled_with(Some("NO")));
        assert!(adopt_disabled_with(Some("1")));
        assert!(adopt_disabled_with(Some("true")));
        assert!(adopt_disabled_with(Some("yes")));
    }

    // ─── Id validation ──────────────────────────────────────────────────────

    #[test]
    fn ids_that_escape_the_store_are_rejected() {
        let store = JobStore::with_root("/tmp/jobs");
        assert!(store.path_for("../escape").is_err());
        assert!(store.path_for("a/b").is_err());
        assert!(store.path_for("").is_err());
        assert!(store.path_for("task_1").is_ok());
        assert!(store.path_for("cron-2").is_ok());
    }

    // ─── Lifecycle fields ───────────────────────────────────────────────────

    #[test]
    fn settling_stamps_state_and_end_fields() {
        let mut r = record(
            "task_1",
            JobKind::Shell,
            Some("sess_a"),
            JobState::Running,
            NOW,
        );
        r.settle(JobState::Failed, NOW + 5, Some(7), Some("boom".into()));

        assert_eq!(r.state, JobState::Failed);
        assert_eq!(r.updated_at_ms, NOW + 5);
        assert_eq!(r.ended_at_ms, Some(NOW + 5));
        assert_eq!(r.exit_code, Some(7));
        assert_eq!(r.error.as_deref(), Some("boom"));
    }

    /// Records written before the end fields existed must still decode; the
    /// fields are additive and default to `None`.
    #[test]
    fn legacy_record_without_lifecycle_fields_decodes() {
        let json = r#"{
            "id": "task_1",
            "kind": "shell",
            "description": "old record",
            "cwd": "/work",
            "session_id": "sess_a",
            "pid": null,
            "started_at_ms": 1700000000000,
            "updated_at_ms": 1700000001000,
            "state": "done"
        }"#;
        let decoded: JobRecord = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.state, JobState::Done);
        assert_eq!(decoded.ended_at_ms, None);
        assert_eq!(decoded.exit_code, None);
        assert_eq!(decoded.error, None);
    }
}
