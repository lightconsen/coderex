//! Task data structures and state machine.
//!
//! Phase 10.1–10.2: Defines `TaskKind`, `TaskState`, `TaskHandle`, and `TaskManager`
//! for managing local shell commands, sub-agents, and other background tasks.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

// ─── TaskKind ───────────────────────────────────────────────────────────────

/// The kind of task being tracked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskKind {
    /// A local shell command running as a subprocess.
    LocalShell {
        /// Command string that was executed.
        command: String,
    },
    /// A sub-agent running in a separate process.
    LocalAgent {
        /// Agent type descriptor.
        agent_type: String,
        /// PID of the agent process.
        pid: Option<u32>,
    },
    /// The main interactive session.
    MainSession,
    /// A background "dream" task (e.g., research, planning).
    Dream {
        /// Description of the dream task.
        description: String,
    },
    /// An in-process teammate (not a separate process).
    InProcessTeammate {
        /// Name of the teammate.
        name: String,
    },
}

// ─── TaskState ──────────────────────────────────────────────────────────────

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Task is queued, not yet started.
    Pending,
    /// Task is currently running.
    Running,
    /// Task completed successfully.
    Completed,
    /// Task failed with an error.
    Failed,
    /// Task was stopped by the user or system.
    Stopped,
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Errors from task operations.
#[derive(Debug, Error)]
pub enum TaskError {
    /// Task not found.
    #[error("task not found: {0}")]
    NotFound(String),
    /// Invalid state transition.
    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition {
        /// Source state.
        from: TaskState,
        /// Target state.
        to: TaskState,
    },
    /// Task already cancelled/stopped.
    #[error("task already terminated: {0}")]
    AlreadyTerminated(String),
}

// ─── TaskHandle ─────────────────────────────────────────────────────────────

/// A handle to a running task.
///
/// Provides access to task state, output, and lifecycle control.
#[derive(Debug, Clone, Serialize)]
pub struct TaskHandle {
    /// Unique task identifier.
    pub id: String,
    /// The kind of task.
    pub kind: TaskKind,
    /// Current lifecycle state.
    pub state: TaskState,
    /// Path to a file containing task output.
    pub output_path: Option<PathBuf>,
    /// Error message if task failed.
    pub error: Option<String>,
    /// Whether this task runs in the background.
    pub is_background: bool,
    /// Exit code (set when task completes).
    pub exit_code: Option<i32>,
}

impl TaskHandle {
    /// Create a new TaskHandle in Pending state.
    #[must_use]
    pub fn new(id: String, kind: TaskKind) -> Self {
        Self {
            id,
            kind,
            state: TaskState::Pending,
            output_path: None,
            error: None,
            is_background: false,
            exit_code: None,
        }
    }

    /// Mark the task as started.
    pub fn start(&mut self) -> Result<(), TaskError> {
        self.transition_to(TaskState::Running)
    }

    /// Mark the task as completed.
    pub fn complete(&mut self, exit_code: Option<i32>) -> Result<(), TaskError> {
        self.transition_to(TaskState::Completed)?;
        self.exit_code = exit_code;
        Ok(())
    }

    /// Mark the task as failed.
    pub fn fail(&mut self, error: String) -> Result<(), TaskError> {
        self.transition_to(TaskState::Failed)?;
        self.error = Some(error);
        Ok(())
    }

    /// Cancel/stop the task.
    pub fn cancel(&mut self) -> Result<(), TaskError> {
        self.transition_to(TaskState::Stopped)?;
        self.error = Some("cancelled".into());
        Ok(())
    }

    /// Get the current task status.
    #[must_use]
    pub const fn get_status(&self) -> TaskState {
        self.state
    }

    /// Get the task output path content.
    ///
    /// Reads the output file if it exists.
    pub fn get_output(&self) -> Result<Option<String>, std::io::Error> {
        if let Some(ref path) = self.output_path {
            if path.exists() {
                return Ok(Some(std::fs::read_to_string(path)?));
            }
        }
        Ok(None)
    }

    /// Transition to a new state, validating the transition.
    fn transition_to(&mut self, new_state: TaskState) -> Result<(), TaskError> {
        if !is_valid_transition(self.state, new_state) {
            return Err(TaskError::InvalidTransition {
                from: self.state,
                to: new_state,
            });
        }
        self.state = new_state;
        Ok(())
    }

    /// Set the output file path for this task.
    pub fn set_output_path(&mut self, path: PathBuf) {
        self.output_path = Some(path);
    }

    /// Check if the task is in a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            TaskState::Completed | TaskState::Failed | TaskState::Stopped
        )
    }
}

/// Validate whether a state transition is allowed.
const fn is_valid_transition(from: TaskState, to: TaskState) -> bool {
    match (from, to) {
        (TaskState::Pending, TaskState::Running) => true,
        (TaskState::Pending, TaskState::Stopped) => true,
        (TaskState::Running, TaskState::Completed) => true,
        (TaskState::Running, TaskState::Failed) => true,
        (TaskState::Running, TaskState::Stopped) => true,
        _ => false,
    }
}

// ─── TaskManager ────────────────────────────────────────────────────────────

/// Manages the lifecycle of tasks.
///
/// Tracks running processes/agents, provides CRUD operations,
/// and handles foreground/background task switching.
#[derive(Debug)]
pub struct TaskManager {
    tasks: HashMap<String, TaskHandle>,
    foreground_task_id: Option<String>,
    id_counter: AtomicU64,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    /// Create a new empty TaskManager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            foreground_task_id: None,
            id_counter: AtomicU64::new(0),
        }
    }

    /// Generate a unique task ID.
    #[must_use]
    pub fn next_id(&self) -> String {
        let n = self.id_counter.fetch_add(1, Ordering::Relaxed);
        format!("task_{n}")
    }

    /// Create a new task and return its handle.
    pub fn create_task(&mut self, kind: TaskKind) -> &TaskHandle {
        let id = self.next_id();
        let handle = TaskHandle::new(id.clone(), kind);
        self.tasks.insert(id.clone(), handle);
        self.tasks.get(&id).unwrap()
    }

    /// Get a task by ID.
    #[must_use]
    pub fn get_task(&self, id: &str) -> Option<&TaskHandle> {
        self.tasks.get(id)
    }

    /// Get a mutable reference to a task by ID.
    pub fn get_task_mut(&mut self, id: &str) -> Option<&mut TaskHandle> {
        self.tasks.get_mut(id)
    }

    /// List all tasks, optionally filtered by state.
    #[must_use]
    pub fn list_tasks(&self, state_filter: Option<TaskState>) -> Vec<&TaskHandle> {
        self.tasks
            .values()
            .filter(|h| state_filter.map_or(true, |s| h.state == s))
            .collect()
    }

    /// Update a task's state.
    pub fn update_task_state(&mut self, id: &str, new_state: TaskState) -> Result<(), TaskError> {
        let handle = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| TaskError::NotFound(id.to_string()))?;
        handle.transition_to(new_state)
    }

    /// Stop a task by ID.
    pub fn stop_task(&mut self, id: &str) -> Result<(), TaskError> {
        let handle = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| TaskError::NotFound(id.to_string()))?;
        handle.cancel()
    }

    /// Get the output of a task.
    pub fn get_task_output(&self, id: &str) -> Result<Option<String>, TaskError> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| TaskError::NotFound(id.to_string()))?;
        handle
            .get_output()
            .map_err(|_| TaskError::NotFound(id.to_string()))
    }

    /// Set a task as the foreground task.
    pub fn set_foreground(&mut self, id: &str) -> Result<(), TaskError> {
        if !self.tasks.contains_key(id) {
            return Err(TaskError::NotFound(id.to_string()));
        }
        if let Some(ref handle) = self.tasks.get(id) {
            if handle.is_terminal() {
                return Err(TaskError::AlreadyTerminated(id.to_string()));
            }
        }
        self.foreground_task_id = Some(id.to_string());
        Ok(())
    }

    /// Remove the current foreground task (switch to background).
    pub fn clear_foreground(&mut self) {
        self.foreground_task_id = None;
    }

    /// Get the current foreground task ID.
    #[must_use]
    pub fn foreground_task_id(&self) -> Option<&str> {
        self.foreground_task_id.as_deref()
    }

    /// Get the foreground task handle.
    #[must_use]
    pub fn foreground_task(&self) -> Option<&TaskHandle> {
        self.foreground_task_id
            .as_deref()
            .and_then(|id| self.tasks.get(id))
    }

    /// Mark a task as running (transition from Pending).
    pub fn mark_running(&mut self, id: &str) -> Result<(), TaskError> {
        let handle = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| TaskError::NotFound(id.to_string()))?;
        handle.start()
    }

    /// Mark a task as completed.
    pub fn mark_completed(&mut self, id: &str, exit_code: Option<i32>) -> Result<(), TaskError> {
        let handle = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| TaskError::NotFound(id.to_string()))?;
        handle.complete(exit_code)
    }

    /// Mark a task as failed.
    pub fn mark_failed(&mut self, id: &str, error: String) -> Result<(), TaskError> {
        let handle = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| TaskError::NotFound(id.to_string()))?;
        handle.fail(error)
    }

    /// Remove a terminated task from the manager.
    pub fn remove_task(&mut self, id: &str) -> Result<TaskHandle, TaskError> {
        if let Some(handle) = self.tasks.get(id) {
            if !handle.is_terminal() {
                return Err(TaskError::InvalidTransition {
                    from: handle.state,
                    to: TaskState::Stopped,
                });
            }
        }
        self.tasks
            .remove(id)
            .ok_or_else(|| TaskError::NotFound(id.to_string()))
    }

    /// Get the number of running tasks.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| h.state == TaskState::Running)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── TaskHandle tests ───────────────────────────────────────────────────

    #[test]
    fn task_handle_starts_pending() {
        let handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        assert_eq!(handle.state, TaskState::Pending);
        assert_eq!(handle.get_status(), TaskState::Pending);
    }

    #[test]
    fn task_handle_valid_transitions() {
        let mut handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        handle.start().unwrap();
        assert_eq!(handle.state, TaskState::Running);

        handle.complete(None).unwrap();
        assert_eq!(handle.state, TaskState::Completed);
        assert!(handle.is_terminal());
    }

    #[test]
    fn task_handle_can_be_cancelled() {
        let mut handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        handle.start().unwrap();
        handle.cancel().unwrap();
        assert_eq!(handle.state, TaskState::Stopped);
        assert!(handle.is_terminal());
    }

    #[test]
    fn task_handle_invalid_completed_from_pending() {
        let mut handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        // Can't go directly from Pending to Completed
        let result = handle.complete(None);
        assert!(result.is_err());
    }

    #[test]
    fn task_handle_invalid_restart() {
        let mut handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        handle.start().unwrap();
        handle.complete(None).unwrap();
        // Can't restart after completion
        let result = handle.start();
        assert!(result.is_err());
    }

    #[test]
    fn task_handle_fail_from_running() {
        let mut handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        handle.start().unwrap();
        handle.fail("something broke".into()).unwrap();
        assert_eq!(handle.state, TaskState::Failed);
        assert_eq!(handle.error, Some("something broke".into()));
    }

    #[test]
    fn task_handle_fail_from_pending() {
        let mut handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        let result = handle.fail("never started".into());
        assert!(result.is_err());
    }

    #[test]
    fn task_handle_get_output_returns_none_without_path() {
        let handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        assert!(handle.get_output().unwrap().is_none());
    }

    #[test]
    fn task_handle_get_output_reads_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("output.txt");
        std::fs::write(&path, "hello world").unwrap();

        let mut handle = TaskHandle::new("t1".into(), TaskKind::MainSession);
        handle.set_output_path(path);
        let output = handle.get_output().unwrap();
        assert_eq!(output, Some("hello world".into()));
    }

    // ─── TaskManager tests ──────────────────────────────────────────────────

    #[test]
    fn manager_starts_empty() {
        let mgr = TaskManager::new();
        assert!(mgr.list_tasks(None).is_empty());
        assert_eq!(mgr.running_count(), 0);
        assert!(mgr.foreground_task().is_none());
    }

    #[test]
    fn manager_create_and_get() {
        let mut mgr = TaskManager::new();
        let kind = TaskKind::LocalShell {
            command: "echo hello".into(),
        };
        let handle = mgr.create_task(kind);
        let id = handle.id.clone();

        let got = mgr.get_task(&id).unwrap();
        assert_eq!(got.id, id);
        assert!(matches!(got.kind, TaskKind::LocalShell { .. }));
    }

    #[test]
    fn manager_list_with_filter() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        mgr.create_task(TaskKind::MainSession);
        mgr.create_task(TaskKind::MainSession);

        let all = mgr.list_tasks(None);
        assert_eq!(all.len(), 3);

        let pending = mgr.list_tasks(Some(TaskState::Pending));
        assert_eq!(pending.len(), 3);
    }

    #[test]
    fn manager_update_task_state() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        let id = mgr.list_tasks(None)[0].id.clone();

        mgr.mark_running(&id).unwrap();
        assert_eq!(mgr.get_task(&id).unwrap().state, TaskState::Running);

        mgr.mark_completed(&id, Some(0)).unwrap();
        assert_eq!(mgr.get_task(&id).unwrap().state, TaskState::Completed);
    }

    #[test]
    fn manager_stop_task() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        let id = mgr.list_tasks(None)[0].id.clone();
        mgr.mark_running(&id).unwrap();

        mgr.stop_task(&id).unwrap();
        assert_eq!(mgr.get_task(&id).unwrap().state, TaskState::Stopped);
    }

    #[test]
    fn manager_stop_nonexistent_errors() {
        let mut mgr = TaskManager::new();
        let result = mgr.stop_task("nope");
        assert!(matches!(result, Err(TaskError::NotFound(_))));
    }

    #[test]
    fn manager_foreground_switching() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        let id = mgr.list_tasks(None)[0].id.clone();

        mgr.set_foreground(&id).unwrap();
        assert_eq!(mgr.foreground_task_id(), Some(id.as_str()));
        assert!(mgr.foreground_task().is_some());

        mgr.clear_foreground();
        assert!(mgr.foreground_task().is_none());
    }

    #[test]
    fn manager_foreground_on_terminated_errors() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        let id = mgr.list_tasks(None)[0].id.clone();
        mgr.mark_running(&id).unwrap();
        mgr.mark_completed(&id, Some(0)).unwrap();

        let result = mgr.set_foreground(&id);
        assert!(matches!(result, Err(TaskError::AlreadyTerminated(_))));
    }

    #[test]
    fn manager_remove_task() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        let id = mgr.list_tasks(None)[0].id.clone();
        mgr.mark_running(&id).unwrap();
        mgr.mark_completed(&id, Some(0)).unwrap();

        let removed = mgr.remove_task(&id).unwrap();
        assert_eq!(removed.id, id);
        assert!(mgr.get_task(&id).is_none());
    }

    #[test]
    fn manager_remove_running_task_errors() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        let id = mgr.list_tasks(None)[0].id.clone();
        mgr.mark_running(&id).unwrap();

        let result = mgr.remove_task(&id);
        assert!(matches!(result, Err(TaskError::InvalidTransition { .. })));
    }

    #[test]
    fn manager_running_count() {
        let mut mgr = TaskManager::new();
        mgr.create_task(TaskKind::MainSession);
        mgr.create_task(TaskKind::MainSession);
        mgr.create_task(TaskKind::MainSession);

        assert_eq!(mgr.running_count(), 0);

        let id = mgr.list_tasks(None)[0].id.clone();
        mgr.mark_running(&id).unwrap();
        assert_eq!(mgr.running_count(), 1);

        mgr.mark_completed(&id, Some(0)).unwrap();
        assert_eq!(mgr.running_count(), 0);
    }

    #[test]
    fn task_ids_are_unique() {
        let mgr = TaskManager::new();
        let id1 = mgr.next_id();
        let id2 = mgr.next_id();
        let id3 = mgr.next_id();
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn is_valid_transition_correct() {
        assert!(is_valid_transition(TaskState::Pending, TaskState::Running));
        assert!(is_valid_transition(TaskState::Pending, TaskState::Stopped));
        assert!(is_valid_transition(
            TaskState::Running,
            TaskState::Completed
        ));
        assert!(is_valid_transition(TaskState::Running, TaskState::Failed));
        assert!(is_valid_transition(TaskState::Running, TaskState::Stopped));
        assert!(!is_valid_transition(
            TaskState::Completed,
            TaskState::Running
        ));
        assert!(!is_valid_transition(TaskState::Failed, TaskState::Pending));
        assert!(!is_valid_transition(TaskState::Stopped, TaskState::Running));
        assert!(!is_valid_transition(
            TaskState::Pending,
            TaskState::Completed
        ));
    }

    #[test]
    fn task_state_display() {
        assert_eq!(TaskState::Pending.to_string(), "pending");
        assert_eq!(TaskState::Running.to_string(), "running");
        assert_eq!(TaskState::Completed.to_string(), "completed");
        assert_eq!(TaskState::Failed.to_string(), "failed");
        assert_eq!(TaskState::Stopped.to_string(), "stopped");
    }
}
