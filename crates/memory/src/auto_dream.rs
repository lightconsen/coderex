//! Auto dream — background session consolidation triggered by thresholds.
//!
//! When the number of sessions or elapsed time exceeds configured thresholds,
//! a read-only "dream" sub-agent is spawned to analyze recent sessions and
//! produce structured summaries that are written to the project memory directory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Configuration for auto dream triggering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamConfig {
    /// Trigger a dream after this many new sessions since last dream.
    pub session_threshold: usize,
    /// Trigger a dream after this much time since last dream (in hours).
    pub time_threshold_hours: i64,
    /// Whether auto dream is enabled.
    pub enabled: bool,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            session_threshold: 10,
            time_threshold_hours: 24,
            enabled: true,
        }
    }
}

/// Result of a dream threshold check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DreamCheckResult {
    /// Thresholds not yet met.
    NotReady,
    /// Thresholds met — should spawn a dream agent.
    ShouldDream {
        /// Number of sessions since last dream.
        sessions_since_last: usize,
        /// Hours since last dream.
        hours_since_last: i64,
    },
    /// A dream is already in progress.
    InProgress,
}

/// A dream session record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamSession {
    /// When the dream started.
    pub started_at: DateTime<Utc>,
    /// When the dream completed (None if still running).
    pub completed_at: Option<DateTime<Utc>>,
    /// Number of sessions analyzed during this dream.
    pub sessions_analyzed: usize,
    /// Dream output/summary written to memory.
    pub summary: String,
    /// Whether the dream completed successfully.
    pub success: bool,
}

/// Auto dream manager — tracks thresholds and manages dream sessions.
#[derive(Debug)]
pub struct AutoDream {
    /// Project memory directory where dream artifacts are written.
    pub memory_dir: PathBuf,
    /// Current dream configuration.
    pub config: DreamConfig,
    /// Path to the dream state file.
    pub state_path: PathBuf,
}

/// Persisted dream state for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DreamState {
    /// When the last successful dream completed.
    last_dream_completed: Option<DateTime<Utc>>,
    /// Number of sessions since last dream.
    sessions_since_last_dream: usize,
    /// List of completed dream sessions.
    dream_history: Vec<DreamSession>,
}

impl AutoDream {
    /// Create a new auto dream manager.
    #[must_use]
    pub fn new(memory_dir: PathBuf, config: DreamConfig) -> Self {
        let state_path = memory_dir.join(".dream_state.json");
        Self {
            memory_dir,
            config,
            state_path,
        }
    }

    /// Load or initialize dream state.
    fn load_state(&self) -> DreamState {
        if self.state_path.exists()
            && let Ok(content) = std::fs::read_to_string(&self.state_path)
            && let Ok(state) = serde_json::from_str::<DreamState>(&content)
        {
            return state;
        }
        DreamState::default()
    }

    /// Persist dream state to disk.
    fn save_state(&self, state: &DreamState) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.memory_dir)?;
        let serialized = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
        std::fs::write(&self.state_path, serialized)
    }

    /// Record a new session, incrementing the session counter.
    pub fn record_session(&self) -> std::io::Result<()> {
        let mut state = self.load_state();
        state.sessions_since_last_dream += 1;
        self.save_state(&state)
    }

    /// Check if dream thresholds are met.
    ///
    /// Returns `DreamCheckResult::ShouldDream` if either the session count
    /// or elapsed time exceeds the configured threshold.
    #[must_use]
    pub fn check_threshold(&self) -> DreamCheckResult {
        if !self.config.enabled {
            return DreamCheckResult::NotReady;
        }

        let state = self.load_state();
        let now = Utc::now();

        let sessions_since = state.sessions_since_last_dream;
        let hours_since = state
            .last_dream_completed
            .map_or(i64::MAX, |t| now.signed_duration_since(t).num_hours());

        let session_met = sessions_since >= self.config.session_threshold;
        let time_met = hours_since >= self.config.time_threshold_hours;

        if session_met || time_met {
            DreamCheckResult::ShouldDream {
                sessions_since_last: sessions_since,
                hours_since_last: hours_since,
            }
        } else {
            DreamCheckResult::NotReady
        }
    }

    /// Start a new dream session.
    ///
    /// Records the start time and marks dream as in progress.
    pub fn start_dream(&self) -> std::io::Result<DreamSession> {
        let dream = DreamSession {
            started_at: Utc::now(),
            completed_at: None,
            sessions_analyzed: 0,
            summary: String::new(),
            success: false,
        };
        Ok(dream)
    }

    /// Complete a dream session, writing the summary to the project memory directory.
    ///
    /// Writes the dream output to `<memory_dir>/dreams/<timestamp>.md`.
    /// Resets the session counter on successful completion.
    pub fn complete_dream(
        &self,
        mut dream: DreamSession,
        summary: &str,
        sessions_analyzed: usize,
    ) -> std::io::Result<DreamSession> {
        dream.completed_at = Some(Utc::now());
        dream.summary = summary.to_string();
        dream.sessions_analyzed = sessions_analyzed;
        dream.success = true;

        // Write dream artifact
        let dreams_dir = self.memory_dir.join("dreams");
        std::fs::create_dir_all(&dreams_dir)?;
        let timestamp = dream.started_at.format("%Y%m%d_%H%M%S");
        let artifact_path = dreams_dir.join(format!("{timestamp}.md"));
        std::fs::write(&artifact_path, &dream.summary)?;

        // Update state
        let mut state = self.load_state();
        state.last_dream_completed = dream.completed_at;
        state.sessions_since_last_dream = 0;
        state.dream_history.push(dream.clone());
        // Keep only last 10 entries to prevent state file bloat
        if state.dream_history.len() > 10 {
            let drain_idx = state.dream_history.len() - 10;
            state.dream_history.drain(..drain_idx);
        }
        self.save_state(&state)?;

        Ok(dream)
    }

    /// Get the list of completed dream sessions.
    #[must_use]
    pub fn dream_history(&self) -> Vec<DreamSession> {
        self.load_state().dream_history
    }

    /// Spawn a background polling loop that checks dream thresholds
    /// at the given interval and sends completed dream sessions to the channel.
    ///
    /// The loop runs until the shutdown signal is triggered.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    /// let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    /// dream.spawn_watcher(tx, Duration::from_secs(60), shutdown_rx).await;
    /// ```
    pub async fn spawn_watcher(
        &self,
        tx: tokio::sync::mpsc::Sender<DreamSession>,
        poll_interval: std::time::Duration,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::debug!("auto dream watcher shutting down");
                    break;
                }
                _ = interval.tick() => {
                    if !self.config.enabled {
                        continue;
                    }
                    match self.check_threshold() {
                        DreamCheckResult::ShouldDream { sessions_since_last, .. } => {
                            let dream = match self.start_dream() {
                                Ok(d) => d,
                                Err(e) => {
                                    tracing::error!("failed to start dream: {e}");
                                    continue;
                                }
                            };
                            let completed = match self.complete_dream(
                                dream,
                                &format!("Auto-dream: analyzed {sessions_since_last} sessions"),
                                sessions_since_last,
                            ) {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::error!("failed to complete dream: {e}");
                                    continue;
                                }
                            };
                            if tx.send(completed).await.is_err() {
                                tracing::debug!("dream watcher receiver dropped, stopping");
                                break;
                            }
                        }
                        DreamCheckResult::NotReady | DreamCheckResult::InProgress => {}
                    }
                }
            }
        }
    }

    /// Spawn a background watcher that calls an LLM to produce dream summaries.
    ///
    /// Similar to [`spawn_watcher`](Self::spawn_watcher) but instead of
    /// generating a placeholder summary, it sends the dream prompt to an
    /// [`ApiClient`](coderex_core::api_client::ApiClient) and uses the
    /// LLM response as the dream summary.
    ///
    /// # Arguments
    /// * `tx` — Channel to send completed dream sessions.
    /// * `poll_interval` — How often to check thresholds.
    /// * `shutdown` — Signal to stop the watcher.
    /// * `api_client` — LLM API client for generating the summary.
    /// * `model` — Model to use for dream analysis.
    /// * `recent_sessions` — Session summaries to include in the prompt.
    pub async fn spawn_watcher_with_model(
        &self,
        tx: tokio::sync::mpsc::Sender<DreamSession>,
        poll_interval: std::time::Duration,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
        api_client: &dyn coderex_core::api_client::ApiClient,
        model: &coderex_core::types::ModelId,
        recent_sessions: Vec<String>,
    ) {
        let mut interval = tokio::time::interval(poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::debug!("auto dream watcher (model) shutting down");
                    break;
                }
                _ = interval.tick() => {
                    if !self.config.enabled {
                        continue;
                    }
                    match self.check_threshold() {
                        DreamCheckResult::ShouldDream { sessions_since_last, .. } => {
                            let dream = match self.start_dream() {
                                Ok(d) => d,
                                Err(e) => {
                                    tracing::error!("failed to start dream: {e}");
                                    continue;
                                }
                            };

                            let summaries: Vec<&str> = recent_sessions.iter().map(std::string::String::as_str).collect();
                            let prompt = self.generate_dream_prompt(&summaries);

                            // Call LLM
                            let messages = vec![serde_json::json!({
                                "role": "user",
                                "content": prompt,
                            })];

                            match api_client.create_message("", &messages, model, 4096, &[]).await {
                                Ok(response) => {
                                    let completed = match self.complete_dream(
                                        dream,
                                        &response.content,
                                        sessions_since_last,
                                    ) {
                                        Ok(c) => c,
                                        Err(e) => {
                                            tracing::error!("failed to complete dream: {e}");
                                            continue;
                                        }
                                    };
                                    if tx.send(completed).await.is_err() {
                                        tracing::debug!("dream watcher receiver dropped, stopping");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("dream LLM call failed: {e}");
                                }
                            }
                        }
                        DreamCheckResult::NotReady | DreamCheckResult::InProgress => {}
                    }
                }
            }
        }
    }

    /// Generate dream prompt content for the sub-agent.
    ///
    /// This produces the instructions that would be given to a read-only
    /// forked agent to perform background session analysis and summarization.
    #[must_use]
    pub fn generate_dream_prompt(&self, session_summaries: &[&str]) -> String {
        let mut prompt = String::from(
            "You are analyzing recent coding sessions to produce a structured summary.\n\
            Your task is to identify patterns, decisions, and learnings.\n\n\
            Please produce the following sections:\n\n\
            1. **Key Decisions** — important architectural or design decisions made\n\
            2. **Code Patterns** — recurring patterns, conventions, or idioms observed\n\
            3. **Unresolved Items** — TODOs, open questions, or incomplete work\n\
            4. **Suggestions** — improvements or refactorings to consider\n\n",
        );

        prompt.push_str("Here are the recent session summaries:\n\n");
        for (i, summary) in session_summaries.iter().enumerate() {
            let _ = write!(prompt, "## Session {}\n{}\n\n", i + 1, summary);
        }

        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dream() -> (AutoDream, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let config = DreamConfig {
            session_threshold: 3,
            time_threshold_hours: 1,
            enabled: true,
        };
        let dream = AutoDream::new(tmp.path().to_path_buf(), config);
        (dream, tmp)
    }

    #[test]
    fn record_session_increments_counter() {
        let (dream, _tmp) = temp_dream();
        dream.record_session().unwrap();
        dream.record_session().unwrap();

        let state = dream.load_state();
        assert_eq!(state.sessions_since_last_dream, 2);
    }

    #[test]
    fn check_threshold_not_ready() {
        let (dream, _tmp) = temp_dream();
        dream.record_session().unwrap();
        // Only 1 session (threshold is 3), and set a fake recent dream completion
        // so the time threshold doesn't trigger
        let mut state = dream.load_state();
        state.last_dream_completed = Some(Utc::now());
        dream.save_state(&state).unwrap();

        let result = dream.check_threshold();
        assert!(matches!(result, DreamCheckResult::NotReady));
    }

    #[test]
    fn check_threshold_session_count_triggers() {
        let (dream, _tmp) = temp_dream();
        dream.record_session().unwrap();
        dream.record_session().unwrap();
        dream.record_session().unwrap();

        let result = dream.check_threshold();
        assert!(matches!(
            result,
            DreamCheckResult::ShouldDream {
                sessions_since_last: 3,
                ..
            }
        ));
    }

    #[test]
    fn check_threshold_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DreamConfig::default();
        let _dream = AutoDream::new(tmp.path().to_path_buf(), config.clone());
        let disabled_config = DreamConfig {
            enabled: false,
            ..config
        };
        let dream2 = AutoDream::new(tmp.path().join("2"), disabled_config);

        // Even with many sessions, disabled means not ready
        for _ in 0..100 {
            dream2.record_session().unwrap();
        }
        assert!(matches!(
            dream2.check_threshold(),
            DreamCheckResult::NotReady
        ));
    }

    #[test]
    fn complete_dream_resets_counter() {
        let (dream, _tmp) = temp_dream();
        for _ in 0..5 {
            dream.record_session().unwrap();
        }

        let dream_session = dream.start_dream().unwrap();
        dream.complete_dream(dream_session, "summary", 5).unwrap();

        let state = dream.load_state();
        assert_eq!(state.sessions_since_last_dream, 0);
        assert!(state.last_dream_completed.is_some());
    }

    #[test]
    fn dream_artifact_written() {
        let (dream, tmp) = temp_dream();
        let dream_session = dream.start_dream().unwrap();
        dream
            .complete_dream(dream_session, "# Dream Summary\nKey finding: Rust", 3)
            .unwrap();

        let dreams_dir = tmp.path().join("dreams");
        assert!(dreams_dir.exists());

        let entries: Vec<_> = std::fs::read_dir(&dreams_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);

        let entry = entries[0].as_ref().unwrap();
        let content = std::fs::read_to_string(entry.path()).unwrap();
        assert_eq!(content, "# Dream Summary\nKey finding: Rust");
    }

    #[test]
    fn dream_history_limited_to_10() {
        let (dream, _tmp) = temp_dream();

        for i in 0..15 {
            dream.record_session().unwrap();
            dream.record_session().unwrap();
            dream.record_session().unwrap();
            let ds = dream.start_dream().unwrap();
            dream
                .complete_dream(ds, &format!("Dream {}", i), 3)
                .unwrap();
        }

        let history = dream.dream_history();
        assert_eq!(history.len(), 10);
    }

    #[test]
    fn generate_dream_prompt_contains_sessions() {
        let (dream, _tmp) = temp_dream();
        let prompt =
            dream.generate_dream_prompt(&["Session A: fixed auth bug", "Session B: added tests"]);
        assert!(prompt.contains("Session A: fixed auth bug"));
        assert!(prompt.contains("Session B: added tests"));
        assert!(prompt.contains("Key Decisions"));
        assert!(prompt.contains("Suggestions"));
    }

    #[test]
    fn dream_state_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DreamConfig {
            session_threshold: 5,
            time_threshold_hours: 2,
            enabled: true,
        };
        {
            let dream = AutoDream::new(tmp.path().to_path_buf(), config.clone());
            dream.record_session().unwrap();
            dream.record_session().unwrap();
        }

        // Load in a new instance
        let dream2 = AutoDream::new(tmp.path().to_path_buf(), config);
        let state = dream2.load_state();
        assert_eq!(state.sessions_since_last_dream, 2);
    }

    #[test]
    fn check_threshold_time_based() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DreamConfig {
            session_threshold: 100,  // High session threshold so time triggers first
            time_threshold_hours: 0, // Trigger immediately if no previous dream
            enabled: true,
        };
        let dream = AutoDream::new(tmp.path().to_path_buf(), config);

        let result = dream.check_threshold();
        // With no previous dream, hours_since_last = MAX which exceeds any threshold
        assert!(matches!(result, DreamCheckResult::ShouldDream { .. }));
    }

    #[tokio::test]
    async fn spawn_watcher_triggers_on_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DreamConfig {
            session_threshold: 1, // Trigger immediately
            time_threshold_hours: 0,
            enabled: true,
        };
        let dream = AutoDream::new(tmp.path().to_path_buf(), config);

        // Record enough sessions to trigger
        dream.record_session().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        // Run the watcher with a very short poll interval
        let poll_interval = std::time::Duration::from_millis(50);
        let handle = tokio::spawn(async move {
            dream.spawn_watcher(tx, poll_interval, shutdown_rx).await;
        });

        // Should receive a dream session within a second
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for dream")
            .expect("channel closed");
        assert!(received.success);
        assert_eq!(received.sessions_analyzed, 1);

        // Signal shutdown and wait
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    }

    #[tokio::test]
    async fn spawn_watcher_with_model_calls_llm() {
        use crate::ai_workflow::tests::MockApi;

        let tmp = tempfile::tempdir().unwrap();
        let config = DreamConfig {
            session_threshold: 1,
            time_threshold_hours: 0,
            enabled: true,
        };
        let dream = AutoDream::new(tmp.path().to_path_buf(), config);
        dream.record_session().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let api = MockApi::new("AI dream summary: found 3 patterns");
        let model = coderex_core::types::ModelId::sonnet();
        let sessions = vec!["Session 1: fixed auth".to_string()];

        let poll_interval = std::time::Duration::from_millis(50);
        let handle = tokio::spawn(async move {
            dream
                .spawn_watcher_with_model(tx, poll_interval, shutdown_rx, &api, &model, sessions)
                .await;
        });

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for dream")
            .expect("channel closed");
        assert!(received.success);
        assert!(received.summary.contains("AI dream summary"));

        let _ = shutdown_tx.send(());
        let _ = handle.await;
    }
}
