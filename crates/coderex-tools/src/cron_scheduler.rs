//! CronScheduler — in-memory cron job scheduler with file persistence.

use chrono::{Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub const CRON_SCHEDULER_KEY: &str = "cron_scheduler";

const JOB_FILE: &str = ".claude/scheduled_tasks.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
    pub created_at: String,
    pub last_fire: Option<String>,
    pub expires_at: Option<String>,
}

impl CronJob {
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
        Self { id, cron, prompt, recurring, durable, created_at: now, last_fire: None, expires_at }
    }
}

static LOCAL_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct CronScheduler {
    jobs: RwLock<HashMap<String, CronJob>>,
    storage_path: RwLock<Option<PathBuf>>,
}

impl CronScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_storage(mut self, path: PathBuf) -> Self {
        *self.storage_path.write().unwrap() = Some(path);
        self
    }

    /// Load durable jobs from disk if the storage file exists.
    pub fn load(&self) -> Result<(), String> {
        let guard = self.storage_path.read().unwrap();
        let path = guard.as_ref().ok_or_else(|| "no storage path set".to_string())?;
        if !path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(path).map_err(|e| format!("failed to read: {e}"))?;
        let jobs: Vec<CronJob> =
            serde_json::from_str(&content).map_err(|e| format!("failed to parse: {e}"))?;
        let mut map = self.jobs.write().unwrap();
        for job in jobs {
            if job.durable {
                map.insert(job.id.clone(), job);
            }
        }
        Ok(())
    }

    /// Persist durable jobs to disk.
    pub fn save(&self) -> Result<(), String> {
        let guard = self.storage_path.read().unwrap();
        let path = guard.as_ref().ok_or_else(|| "no storage path set".to_string())?;
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

    pub fn create(&self, job: CronJob) -> String {
        let id = job.id.clone();
        if job.durable {
            self.jobs.write().unwrap().insert(id.clone(), job);
            let _ = self.save();
        } else {
            self.jobs.write().unwrap().insert(id.clone(), job);
        }
        id
    }

    pub fn delete(&self, id: &str) -> bool {
        let removed = self.jobs.write().unwrap().remove(id).is_some();
        if removed {
            let _ = self.save();
        }
        removed
    }

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

    pub fn mark_fired(&self, id: &str) {
        let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut map = self.jobs.write().unwrap();
        if let Some(job) = map.get_mut(id) {
            job.last_fire = Some(now.clone());
            // One-shot jobs are deleted after firing
            if !job.recurring {
                map.remove(id);
            }
        }
        drop(map);
        let _ = self.save();
    }

    pub fn get_prompt(&self, id: &str) -> Option<String> {
        self.jobs.read().unwrap().get(id).map(|j| j.prompt.clone())
    }
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

fn match_single(spec: &str, value: i64, min: i64, max: i64) -> bool {
    // Handle step: */N or range/N
    if let Some((base, step_str)) = spec.split_once('/') {
        let step: i64 = match step_str.parse() {
            Ok(n) if n > 0 => n,
            _ => return false,
        };
        let range_start = if base == "*" {
            min
        } else if let Some((s, e)) = base.split_once('-') {
            let s: i64 = match s.parse() { Ok(n) => n, _ => return false };
            let e: i64 = match e.parse() { Ok(n) => n, _ => return false };
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
        let s: i64 = match s.parse() { Ok(n) => n, _ => return false };
        let e: i64 = match e.parse() { Ok(n) => n, _ => return false };
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
}
