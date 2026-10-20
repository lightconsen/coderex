//! Cron expression parsing, scheduling, and job execution.
//!
//! Wraps the `cron` crate for cron expression parsing and next-trigger calculation.
//! Provides [`CronRunner`] for executing scheduled commands in a background task.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::{RwLock, mpsc};

/// A single cron job.
#[derive(Debug, Clone)]
pub struct CronJob {
    /// Unique job identifier.
    pub id: String,
    /// Cron schedule.
    pub schedule: CronSchedule,
    /// Shell command to execute.
    pub command: String,
    /// Whether the job is enabled.
    pub enabled: bool,
}

/// Runs cron jobs in a background task.
#[derive(Debug)]
pub struct CronRunner {
    jobs: Arc<RwLock<Vec<CronJob>>>,
    shutdown_tx: Option<mpsc::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl CronRunner {
    /// Create a new cron runner with no jobs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(Vec::new())),
            shutdown_tx: None,
            handle: None,
        }
    }

    /// Add a job to the runner.
    pub async fn add_job(&self, job: CronJob) {
        self.jobs.write().await.push(job);
    }

    /// Remove a job by id. Returns `true` if the job was found and removed.
    pub async fn remove_job(&self, id: &str) -> bool {
        let mut jobs = self.jobs.write().await;
        let Some(pos) = jobs.iter().position(|j| j.id == id) else {
            return false;
        };
        jobs.remove(pos);
        true
    }

    /// List all jobs.
    #[must_use]
    pub async fn list_jobs(&self) -> Vec<CronJob> {
        self.jobs.read().await.clone()
    }

    /// Start the background execution loop.
    ///
    /// Does nothing if already started.
    pub fn start(&mut self) {
        if self.handle.is_some() {
            return;
        }
        let (tx, mut rx) = mpsc::channel(1);
        self.shutdown_tx = Some(tx);

        let jobs = Arc::clone(&self.jobs);
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            let mut last_run: HashMap<String, DateTime<Utc>> = HashMap::new();

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = Utc::now();
                        let jobs_guard = jobs.read().await;
                        let mut ran_any = false;
                        for job in jobs_guard.iter() {
                            if !job.enabled {
                                continue;
                            }
                            // A job with no `last_run` entry is being seen for
                            // the first time; seeding it avoids firing
                            // immediately on startup.
                            let should_run = last_run.get(&job.id).is_some_and(|last| {
                                job.schedule
                                    .next_after(last)
                                    .is_ok_and(|next| next <= now)
                            });
                            if should_run {
                                let cmd = job.command.clone();
                                let id = job.id.clone();
                                let job_id_spawn = id.clone();
                                tokio::spawn(async move {
                                    tracing::info!(job_id = %job_id_spawn, command = %cmd, "executing cron job");
                                    let parts: Vec<&str> = cmd.split_whitespace().collect();
                                    if parts.is_empty() {
                                        tracing::warn!(job_id = %job_id_spawn, "empty cron command");
                                        return;
                                    }
                                    let mut command = Command::new(parts[0]);
                                    command.args(&parts[1..]);
                                    match command.output().await {
                                        Ok(output) => {
                                            if output.status.success() {
                                                tracing::info!(job_id = %job_id_spawn, "cron job completed");
                                            } else {
                                                let stderr = String::from_utf8_lossy(&output.stderr);
                                                tracing::warn!(job_id = %job_id_spawn, stderr = %stderr, "cron job failed");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(job_id = %job_id_spawn, error = %e, "cron job spawn failed");
                                        }
                                    }
                                });
                                last_run.insert(id, now);
                                ran_any = true;
                            } else if !last_run.contains_key(&job.id) {
                                last_run.insert(job.id.clone(), now);
                            }
                        }
                        drop(jobs_guard);
                        if ran_any {
                            // Small cooldown to avoid immediate re-fire
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                    _ = rx.recv() => {
                        tracing::info!("cron runner shutting down");
                        break;
                    }
                }
            }
        });
        self.handle = Some(handle);
    }

    /// Stop the background execution loop.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Default for CronRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from cron parsing and scheduling.
#[derive(Debug, Error)]
pub enum CronError {
    /// Invalid cron expression.
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),
    /// No future schedule time found.
    #[error("no future schedule time found")]
    NoFutureSchedule,
    /// Invalid datetime.
    #[error("invalid datetime: {0}")]
    InvalidDateTime(String),
}

/// A parsed cron schedule.
#[derive(Debug, Clone)]
pub struct CronSchedule {
    inner: cron::Schedule,
    expression: String,
}

impl CronSchedule {
    /// Parse a cron expression (5-field: min hour dom month dow).
    ///
    /// Supported formats:
    /// - `* * * * *` — every minute
    /// - `*/5 * * * *` — every 5 minutes
    /// - `0 9 * * *` — every day at 9:00
    /// - `0 9 * * 1-5` — weekdays at 9:00
    /// - `30 14 28 2 *` — Feb 28 at 14:30
    pub fn new(expression: &str) -> Result<Self, CronError> {
        // Convert 5-field cron to cron crate's expected format (seconds + 6 fields)
        let six_field = format!("0 {expression}");
        let inner = cron::Schedule::from_str(&six_field)
            .map_err(|e| CronError::InvalidExpression(e.to_string()))?;

        Ok(Self {
            inner,
            expression: expression.to_string(),
        })
    }

    /// Get the next scheduled time from now.
    pub fn next_from_now(&self) -> Result<DateTime<Utc>, CronError> {
        self.next_after(&Utc::now())
    }

    /// Get the next scheduled time after the given datetime.
    pub fn next_after(&self, after: &DateTime<Utc>) -> Result<DateTime<Utc>, CronError> {
        self.inner
            .after(after)
            .next()
            .ok_or(CronError::NoFutureSchedule)
    }

    /// Get the next N scheduled times from now.
    pub fn next_n_from_now(&self, n: usize) -> Result<Vec<DateTime<Utc>>, CronError> {
        self.next_n_after(&Utc::now(), n)
    }

    /// Get the next N scheduled times after the given datetime.
    pub fn next_n_after(
        &self,
        after: &DateTime<Utc>,
        n: usize,
    ) -> Result<Vec<DateTime<Utc>>, CronError> {
        Ok(self.inner.after(after).take(n).collect())
    }

    /// Check if a datetime matches this schedule.
    #[must_use]
    pub fn matches(&self, dt: &DateTime<Utc>) -> bool {
        self.inner.includes(*dt)
    }

    /// Get the original cron expression.
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Get a human-readable description of the schedule.
    #[must_use]
    pub fn description(&self) -> String {
        describe_cron(&self.expression)
    }
}

/// Validate a cron expression without creating a schedule.
pub fn validate_cron(expression: &str) -> Result<(), CronError> {
    CronSchedule::new(expression)?;
    Ok(())
}

/// Calculate the next trigger time for a cron expression.
pub fn next_cron_time(expression: &str, after: &DateTime<Utc>) -> Result<DateTime<Utc>, CronError> {
    let schedule = CronSchedule::new(expression)?;
    schedule.next_after(after)
}

/// Generate a human-readable description for a cron expression.
fn describe_cron(expr: &str) -> String {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return "unknown schedule".to_string();
    }

    let (minute, hour, dom, month, dow) = (parts[0], parts[1], parts[2], parts[3], parts[4]);

    // Common patterns
    match (minute, hour, dom, month, dow) {
        ("*", "*", "*", "*", "*") => "every minute".into(),
        (m, "*", "*", "*", "*") if m.starts_with("*/") => {
            format!("every {} minutes", m.strip_prefix("*/").unwrap_or(m))
        }
        ("0", h, "*", "*", "*") => format!("every day at {h}:00"),
        ("0", h, "*", "*", "1-5") => format!("weekdays at {h}:00"),
        ("0", h, "*", "*", "0" | "6") => {
            format!("Sunday/Saturday at {h}:00")
        }
        (m, h, "*", "*", "*") => format!("every day at {h}:{m}"),
        (m, h, d, "*", "*") => format!("day {d} of every month at {h}:{m}"),
        _ => format!("custom schedule: {expr}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn parse_every_minute() {
        let schedule = CronSchedule::new("* * * * *").unwrap();
        let next = schedule.next_from_now().unwrap();
        assert!(next > Utc::now());
    }

    #[test]
    fn parse_every_five_minutes() {
        let schedule = CronSchedule::new("*/5 * * * *").unwrap();
        let nexts = schedule.next_n_from_now(3).unwrap();

        assert_eq!(nexts.len(), 3);
        // Each should be roughly 5 minutes apart
        for window in nexts.windows(2) {
            let diff = window[1] - window[0];
            assert!(diff.num_minutes() >= 4 && diff.num_minutes() <= 6);
        }
    }

    #[test]
    fn parse_daily_at_hour() {
        let schedule = CronSchedule::new("0 9 * * *").unwrap();
        let next = schedule.next_from_now().unwrap();

        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn parse_weekdays() {
        let schedule = CronSchedule::new("0 9 * * MON-FRI").unwrap();
        let nexts = schedule.next_n_from_now(5).unwrap();

        // Verify all scheduled times are weekdays (not Saturday or Sunday)
        // chrono: Mon=1, ..., Sat=6, Sun=7
        for dt in &nexts {
            let weekday = dt.weekday().number_from_monday();
            assert!(
                (1..=5).contains(&weekday),
                "expected weekday, got {weekday} ({:?})",
                dt.weekday()
            );
        }
        // Should have 5 distinct days
        let distinct_days: std::collections::HashSet<_> =
            nexts.iter().map(|dt| dt.date_naive()).collect();
        assert!(distinct_days.len() >= 3);
    }

    #[test]
    fn parse_specific_date() {
        let schedule = CronSchedule::new("30 14 28 2 *").unwrap();
        let nexts = schedule.next_n_from_now(2).unwrap();

        assert_eq!(nexts.len(), 2);
        for dt in &nexts {
            assert_eq!(dt.month(), 2);
            assert_eq!(dt.day(), 28);
            assert_eq!(dt.hour(), 14);
            assert_eq!(dt.minute(), 30);
        }
    }

    #[test]
    fn invalid_cron_returns_error() {
        let result = CronSchedule::new("invalid");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CronError::InvalidExpression(_)
        ));
    }

    #[test]
    fn validate_cron_accepts_valid() {
        assert!(validate_cron("0 * * * *").is_ok());
        assert!(validate_cron("*/10 9-17 * * 1-5").is_ok());
    }

    #[test]
    fn validate_cron_rejects_invalid() {
        assert!(validate_cron("not a cron").is_err());
        assert!(validate_cron("60 25 * * *").is_err());
    }

    #[test]
    fn next_cron_time_helper() {
        let after = Utc::now();
        let next = next_cron_time("0 * * * *", &after).unwrap();
        assert!(next > after);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn schedule_description_common_patterns() {
        assert_eq!(
            CronSchedule::new("* * * * *").unwrap().description(),
            "every minute"
        );
        assert_eq!(
            CronSchedule::new("0 9 * * *").unwrap().description(),
            "every day at 9:00"
        );
        assert_eq!(
            CronSchedule::new("0 9 * * 1-5").unwrap().description(),
            "weekdays at 9:00"
        );
    }

    #[test]
    fn schedule_expression_returns_original() {
        let schedule = CronSchedule::new("30 14 * * *").unwrap();
        assert_eq!(schedule.expression(), "30 14 * * *");
    }

    #[tokio::test]
    async fn cron_runner_lifecycle() {
        let mut runner = CronRunner::new();

        // Initially empty
        assert!(runner.list_jobs().await.is_empty());

        // Add a job
        runner
            .add_job(CronJob {
                id: "test_1".into(),
                schedule: CronSchedule::new("0 * * * *").unwrap(),
                command: "echo hello".into(),
                enabled: true,
            })
            .await;

        let jobs = runner.list_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "test_1");

        // Start and stop without panic
        runner.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        runner.stop().await;

        // Remove the job
        assert!(runner.remove_job("test_1").await);
        assert!(runner.list_jobs().await.is_empty());
        assert!(!runner.remove_job("test_1").await);
    }
}
