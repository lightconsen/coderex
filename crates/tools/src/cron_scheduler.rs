//! CronScheduler — in-memory cron job scheduler with file persistence.

use chrono::{Datelike, Local, Timelike};
use coderex_core::job_record::{JobKind, JobRecord, JobState, JobStore};
use coderex_core::task::now_ms;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Key used to store the cron scheduler in `ToolContext.extra`.
pub const CRON_SCHEDULER_KEY: &str = "cron_scheduler";

#[allow(dead_code)]
const JOB_FILE: &str = ".coderex/scheduled_tasks.json";

/// A scheduled cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Unique job identifier.
    pub id: String,
    /// Cron expression (5-field standard).
    pub cron: String,
    /// Prompt to enqueue when the job fires.
    pub prompt: String,
    /// Whether the job repeats.
    pub recurring: bool,
    /// Whether the job survives process restart.
    pub durable: bool,
    /// ISO-8601 timestamp of creation.
    pub created_at: String,
    /// ISO-8601 timestamp of last execution (if any).
    pub last_fire: Option<String>,
    /// ISO-8601 timestamp after which the job is ignored.
    pub expires_at: Option<String>,
}

impl CronJob {
    /// Create a new cron job.
    pub fn new(cron: String, prompt: String, recurring: bool, durable: bool) -> Self {
        let id = format!("cron-{}", LOCAL_COUNTER.fetch_add(1, Ordering::SeqCst));
        let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let expires_at = if recurring {
            Some(
                (Local::now() + chrono::Duration::days(7))
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            )
        } else {
            None
        };
        Self {
            id,
            cron,
            prompt,
            recurring,
            durable,
            created_at: now,
            last_fire: None,
            expires_at,
        }
    }
}

static LOCAL_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Where durable cron definitions are mirrored as job records.
#[derive(Debug, Clone)]
struct JobContext {
    store: JobStore,
    cwd: PathBuf,
    session_id: String,
}

/// In-memory cron job scheduler with optional file persistence.
#[derive(Debug, Default)]
pub struct CronScheduler {
    jobs: RwLock<HashMap<String, CronJob>>,
    storage_path: RwLock<Option<PathBuf>>,
    /// Set by the session so durable definitions get a `JobRecord` under
    /// `<cwd>/.coderex/jobs`. Without it, cron scheduling works as before and
    /// writes no records.
    job_context: RwLock<Option<JobContext>>,
}

impl CronScheduler {
    /// Create a new scheduler with no storage path.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the filesystem path used for `load` / `save`.
    pub fn with_storage(self, path: PathBuf) -> Self {
        *self.storage_path.write().unwrap() = Some(path);
        self
    }

    /// Mirror durable definitions into the job store under `cwd`.
    ///
    /// Only durable jobs are recorded: a non-durable definition dies with the
    /// process, so claiming it is resumable in the next session would be a
    /// lie. The session id attributes the record for adoption.
    pub fn with_job_records(self, cwd: PathBuf, session_id: impl Into<String>) -> Self {
        self.set_job_records(cwd, session_id);
        self
    }

    /// [`Self::with_job_records`] for a scheduler already shared behind an
    /// `Arc`.
    pub fn set_job_records(&self, cwd: PathBuf, session_id: impl Into<String>) {
        *self.job_context.write().unwrap() = Some(JobContext {
            store: JobStore::new(&cwd),
            cwd,
            session_id: session_id.into(),
        });
    }

    /// Load durable jobs from disk if the storage file exists.
    pub fn load(&self) -> Result<(), String> {
        let guard = self.storage_path.read().unwrap();
        let path = guard
            .as_ref()
            .ok_or_else(|| "no storage path set".to_string())?;
        if !path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(path).map_err(|e| format!("failed to read: {e}"))?;
        let jobs: Vec<CronJob> =
            serde_json::from_str(&content).map_err(|e| format!("failed to parse: {e}"))?;
        let mut map = self.jobs.write().unwrap();
        for job in jobs {
            if job.durable {
                // The definition survived the restart and is being scheduled
                // again, so its record is Running and fresh — regardless of
                // what the previous process left behind.
                self.refresh_job_record(&job);
                map.insert(job.id.clone(), job);
            }
        }
        Ok(())
    }

    /// Persist durable jobs to disk.
    pub fn save(&self) -> Result<(), String> {
        let guard = self.storage_path.read().unwrap();
        let path = guard
            .as_ref()
            .ok_or_else(|| "no storage path set".to_string())?;
        let map = self.jobs.read().unwrap();
        let durable: Vec<&CronJob> = map.values().filter(|j| j.durable).collect();
        let content = serde_json::to_string_pretty(&durable)
            .map_err(|e| format!("failed to serialize: {e}"))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create dir: {e}"))?;
        }
        fs::write(path, content).map_err(|e| format!("failed to write: {e}"))?;
        Ok(())
    }

    /// Add a job to the scheduler.
    pub fn create(&self, job: CronJob) -> String {
        let id = job.id.clone();
        if job.durable {
            self.write_job_record(&job);
            self.jobs.write().unwrap().insert(id.clone(), job);
            let _ = self.save();
        } else {
            self.jobs.write().unwrap().insert(id.clone(), job);
        }
        id
    }

    /// Remove a job by ID.
    ///
    /// Deleting a durable definition is a cancellation: its record is settled
    /// `Done` rather than removed, so it cannot be reported as running by a
    /// later session.
    pub fn delete(&self, id: &str) -> bool {
        let removed = self.jobs.write().unwrap().remove(id).is_some();
        if removed {
            let _ = self.save();
            if let Some(store) = self.record_store() {
                let _ = store.settle(id, JobState::Done, now_ms(), None, None);
            }
        }
        removed
    }

    /// Return all jobs.
    pub fn list(&self) -> Vec<CronJob> {
        self.jobs.read().unwrap().values().cloned().collect()
    }

    /// Check if a job should fire now based on its cron expression.
    pub fn should_fire(&self, id: &str) -> bool {
        let guard = self.jobs.read().unwrap();
        let Some(job) = guard.get(id) else {
            return false;
        };

        // Check expiry
        if let Some(ref expires) = job.expires_at {
            if Local::now().format("%Y-%m-%dT%H:%M:%S").to_string() >= *expires {
                return false;
            }
        }

        match_cron_expr(&job.cron)
    }

    /// Record that a job fired and update persistence.
    pub fn mark_fired(&self, id: &str) {
        let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut map = self.jobs.write().unwrap();
        let mut recurring = true;
        if let Some(job) = map.get_mut(id) {
            job.last_fire = Some(now.clone());
            recurring = job.recurring;
            // One-shot jobs are deleted after firing
            if !job.recurring {
                map.remove(id);
            }
        }
        drop(map);
        let _ = self.save();
        self.record_fired(id, recurring);
    }

    /// Get the prompt for a job by ID.
    pub fn get_prompt(&self, id: &str) -> Option<String> {
        self.jobs.read().unwrap().get(id).map(|j| j.prompt.clone())
    }

    /// Clone the job-record context out from under its lock, so a caller can
    /// do file IO without holding it.
    fn job_context_parts(&self) -> Option<(JobStore, PathBuf, String)> {
        self.job_context
            .read()
            .unwrap()
            .as_ref()
            .map(|ctx| (ctx.store.clone(), ctx.cwd.clone(), ctx.session_id.clone()))
    }

    fn record_store(&self) -> Option<JobStore> {
        self.job_context_parts().map(|(store, _, _)| store)
    }

    /// Write the `Running` record for a durable definition.
    fn write_job_record(&self, job: &CronJob) {
        let Some((store, cwd, session_id)) = self.job_context_parts() else {
            return;
        };
        let mut record = JobRecord::new(
            job.id.clone(),
            JobKind::Cron,
            job_description(job),
            cwd,
            Some(session_id.as_str()),
            now_ms(),
        );
        // The scheduler runs in-process; the owner is the only pid there is.
        record.pid = Some(std::process::id());
        if let Err(error) = store.write(&record) {
            tracing::warn!(
                job = %job.id,
                path = %store.root().display(),
                error = %error,
                "could not write the cron job record"
            );
        }
    }

    /// Bring a durable definition loaded from disk back to `Running`.
    ///
    /// Loading is re-scheduling: the definition is live again, so whatever a
    /// previous process left in the record (an old timestamp, or a terminal
    /// state) must not make a later adopt treat it as stale or finished.
    fn refresh_job_record(&self, job: &CronJob) {
        let Some((store, cwd, session_id)) = self.job_context_parts() else {
            return;
        };
        let now = now_ms();
        let mut record = match store.load(&job.id) {
            Ok(Some(record)) => record,
            Ok(None) => JobRecord::new(
                job.id.clone(),
                JobKind::Cron,
                String::new(),
                &cwd,
                Some(&session_id),
                now,
            ),
            Err(error) => {
                tracing::warn!(job = %job.id, error = %error, "could not read the cron job record");
                return;
            }
        };
        record.kind = JobKind::Cron;
        record.description = job_description(job);
        record.cwd = cwd;
        record.session_id = Some(session_id);
        record.pid = Some(std::process::id());
        record.state = JobState::Running;
        record.touch(now);
        record.ended_at_ms = None;
        record.exit_code = None;
        record.error = None;
        let _ = store.write(&record);
    }

    /// Update the record after a job fires.
    ///
    /// A recurring definition is still scheduled: touch it so it does not age
    /// out of the adopt window. A one-shot definition is finished, so settle
    /// it rather than leaving a record that claims to be running forever.
    fn record_fired(&self, id: &str, recurring: bool) {
        let Some(store) = self.record_store() else {
            return;
        };
        let now = now_ms();
        if recurring {
            if let Ok(Some(mut record)) = store.load(id) {
                if record.state == JobState::Running {
                    record.touch(now);
                    let _ = store.write(&record);
                }
            }
        } else {
            let _ = store.settle(id, JobState::Done, now, Some(0), None);
        }
    }

    /// Spawn a background tokio task that polls jobs every 60 seconds.
    ///
    /// Returns an unbounded receiver that yields `(job_id, prompt)` for each
    /// job that fires. The consumer should call `mark_fired(job_id)` after
    /// handling the prompt.
    pub fn spawn_runner(self: Arc<Self>) -> mpsc::UnboundedReceiver<(String, String)> {
        let (tx, rx) = mpsc::unbounded_channel::<(String, String)>();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                let jobs = self.list();
                for job in jobs {
                    if self.should_fire(&job.id) {
                        if let Some(prompt) = self.get_prompt(&job.id) {
                            let _ = tx.send((job.id.clone(), prompt));
                        }
                    }
                }
            }
        });

        rx
    }
}

/// One-line description of a definition for its job record.
fn job_description(job: &CronJob) -> String {
    const LIMIT: usize = 200;
    let mut prompt: String = job.prompt.chars().take(LIMIT).collect();
    if job.prompt.chars().count() > LIMIT {
        prompt.push('…');
    }
    format!("cron {}: {prompt}", job.cron)
}

/// Match a standard 5-field cron expression against the current local time.
/// Supports: minute, hour, day-of-month, month, day-of-week.
/// Wildcards (*), ranges (1-5), lists (1,3,5), steps (*/5).
fn match_cron_expr(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }

    let now = Local::now();
    let minute = now.minute() as i64;
    let hour = now.hour() as i64;
    let day = now.day() as i64;
    let month = now.month() as i64;
    let weekday = now.weekday().num_days_from_sunday() as i64; // 0=Sun

    let fields = [
        (minute, 0, 59),
        (hour, 0, 23),
        (day, 1, 31),
        (month, 1, 12),
        (weekday, 0, 6),
    ];

    for (i, &(val, min, max)) in fields.iter().enumerate() {
        if !match_field(parts[i], val, min, max) {
            return false;
        }
    }
    true
}

fn match_field(spec: &str, value: i64, min: i64, max: i64) -> bool {
    // Handle comma-separated lists
    for part in spec.split(',') {
        if match_single(part, value, min, max) {
            return true;
        }
    }
    false
}

fn match_single(spec: &str, value: i64, min: i64, _max: i64) -> bool {
    // Handle step: */N or range/N
    if let Some((base, step_str)) = spec.split_once('/') {
        let step: i64 = match step_str.parse() {
            Ok(n) if n > 0 => n,
            _ => return false,
        };
        let range_start = if base == "*" {
            min
        } else if let Some((s, e)) = base.split_once('-') {
            let s: i64 = match s.parse() {
                Ok(n) => n,
                _ => return false,
            };
            let e: i64 = match e.parse() {
                Ok(n) => n,
                _ => return false,
            };
            // value must be in range AND match step from start
            if value < s || value > e {
                return false;
            }
            s
        } else {
            min
        };
        return (value - range_start) % step == 0;
    }

    if spec == "*" {
        return true;
    }

    // Range: N-M
    if let Some((s, e)) = spec.split_once('-') {
        let s: i64 = match s.parse() {
            Ok(n) => n,
            _ => return false,
        };
        let e: i64 = match e.parse() {
            Ok(n) => n,
            _ => return false,
        };
        return (s..=e).contains(&value);
    }

    // Single value
    if let Ok(n) = spec.parse::<i64>() {
        return n == value;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_every_minute() {
        assert!(match_cron_expr("* * * * *"));
    }

    #[test]
    fn test_match_specific_minute() {
        // This test checks the logic — the actual result depends on current time
        let parts: Vec<&str> = "30 * * * *".split_whitespace().collect();
        assert_eq!(parts.len(), 5);
        // Field parsing works; actual match depends on current minute
        assert!(match_field("30", 30, 0, 59));
        assert!(!match_field("30", 15, 0, 59));
    }

    #[test]
    fn test_match_range() {
        assert!(match_field("1-5", 3, 1, 31));
        assert!(!match_field("1-5", 7, 1, 31));
    }

    #[test]
    fn test_match_step() {
        assert!(match_field("*/5", 0, 0, 59));
        assert!(match_field("*/5", 15, 0, 59));
        assert!(!match_field("*/5", 13, 0, 59));
        assert!(match_field("*/15", 0, 0, 59));
        assert!(match_field("*/15", 45, 0, 59));
    }

    #[test]
    fn test_match_list() {
        assert!(match_field("1,3,5", 3, 1, 31));
        assert!(!match_field("1,3,5", 4, 1, 31));
    }

    #[test]
    fn test_scheduler_create_delete() {
        let sched = CronScheduler::new();
        let job = CronJob::new("* * * * *".into(), "hello".into(), false, false);
        let id = sched.create(job);
        assert!(sched.get_prompt(&id).is_some());
        assert!(sched.delete(&id));
        assert!(sched.get_prompt(&id).is_none());
    }

    #[test]
    fn test_scheduler_list() {
        let sched = CronScheduler::new();
        let job = CronJob::new("* * * * *".into(), "test".into(), true, false);
        sched.create(job);
        assert_eq!(sched.list().len(), 1);
    }

    fn scheduler_with_records(dir: &std::path::Path) -> (CronScheduler, JobStore) {
        let store = JobStore::new(dir);
        let sched = CronScheduler::new()
            .with_storage(dir.join("scheduled_tasks.json"))
            .with_job_records(dir.to_path_buf(), "session-alpha");
        (sched, store)
    }

    #[test]
    fn durable_create_writes_running_record() {
        let dir = tempfile::tempdir().unwrap();
        let (sched, store) = scheduler_with_records(dir.path());
        let job = CronJob::new("*/5 * * * *".into(), "check the build".into(), true, true);
        let id = sched.create(job);

        let record = store.load(&id).unwrap().expect("durable job is recorded");
        assert_eq!(record.state, JobState::Running);
        assert_eq!(record.kind, JobKind::Cron);
        assert_eq!(record.session_id.as_deref(), Some("session-alpha"));
        assert!(record.pid.is_some());
        assert!(record.ended_at_ms.is_none());
        assert!(record.description.contains("check the build"));
    }

    #[test]
    fn non_durable_create_writes_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let (sched, store) = scheduler_with_records(dir.path());
        let job = CronJob::new("* * * * *".into(), "session-only".into(), true, false);
        let id = sched.create(job);
        assert!(store.load(&id).unwrap().is_none());
    }

    #[test]
    fn recurring_fire_keeps_record_running() {
        let dir = tempfile::tempdir().unwrap();
        let (sched, store) = scheduler_with_records(dir.path());
        let job = CronJob::new("* * * * *".into(), "every minute".into(), true, true);
        let id = sched.create(job);
        let before = store.load(&id).unwrap().unwrap().updated_at_ms;

        sched.mark_fired(&id);

        let record = store.load(&id).unwrap().unwrap();
        assert_eq!(record.state, JobState::Running);
        assert!(record.ended_at_ms.is_none());
        assert!(record.updated_at_ms >= before);
        // The definition itself is still scheduled.
        assert!(sched.get_prompt(&id).is_some());
    }

    #[test]
    fn one_shot_fire_settles_record_done() {
        let dir = tempfile::tempdir().unwrap();
        let (sched, store) = scheduler_with_records(dir.path());
        let job = CronJob::new("*/5 * * * *".into(), "run once".into(), false, true);
        let id = sched.create(job);

        sched.mark_fired(&id);

        let record = store.load(&id).unwrap().unwrap();
        assert_eq!(record.state, JobState::Done);
        assert_eq!(record.exit_code, Some(0));
        assert!(record.ended_at_ms.is_some());
        // A one-shot job is removed from the schedule.
        assert!(sched.get_prompt(&id).is_none());
    }

    #[test]
    fn delete_settles_record_done() {
        let dir = tempfile::tempdir().unwrap();
        let (sched, store) = scheduler_with_records(dir.path());
        let job = CronJob::new("* * * * *".into(), "cancel me".into(), true, true);
        let id = sched.create(job);

        assert!(sched.delete(&id));

        let record = store.load(&id).unwrap().unwrap();
        assert_eq!(record.state, JobState::Done);
        assert!(record.ended_at_ms.is_some());
    }

    #[test]
    fn load_refreshes_stale_record_to_running() {
        let dir = tempfile::tempdir().unwrap();
        let (sched, store) = scheduler_with_records(dir.path());
        let job = CronJob::new("* * * * *".into(), "survivor".into(), true, true);
        let id = sched.create(job);

        // Simulate a previous process that stopped leaving a stale record.
        let mut record = store.load(&id).unwrap().unwrap();
        record.touch(1);
        record.started_at_ms = 1;
        store.write(&record).unwrap();

        sched.load().unwrap();

        let refreshed = store.load(&id).unwrap().unwrap();
        assert_eq!(refreshed.state, JobState::Running);
        assert!(refreshed.updated_at_ms > 1);
        assert_eq!(refreshed.session_id.as_deref(), Some("session-alpha"));
    }
}
