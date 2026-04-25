//! Coordinator mode for orchestrating multi-agent workflows.
//!
//! Phase 11.2: Implements `CoordinatorState` with a four-phase flow:
//! Research → Synthesis → Implementation → Verification.
//! Also provides CoordinatorMode tool whitelist logic.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── CoordinatorPhase ──────────────────────────────────────────────────────

/// Phase in the coordinator's four-stage workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorPhase {
    /// Research: gather information, understand the codebase.
    Research,
    /// Synthesis: analyze findings, design a plan.
    Synthesis,
    /// Implementation: execute the plan.
    Implementation,
    /// Verification: validate the result.
    Verification,
}

impl CoordinatorPhase {
    /// Return the next phase in the workflow.
    ///
    /// Returns `None` after the final phase (Verification).
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::Research => Some(Self::Synthesis),
            Self::Synthesis => Some(Self::Implementation),
            Self::Implementation => Some(Self::Verification),
            Self::Verification => None,
        }
    }

    /// Whether this is the initial phase.
    #[must_use]
    pub const fn is_first(self) -> bool {
        matches!(self, Self::Research)
    }

    /// Whether this is the final phase.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Verification)
    }
}

impl std::fmt::Display for CoordinatorPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Research => write!(f, "research"),
            Self::Synthesis => write!(f, "synthesis"),
            Self::Implementation => write!(f, "implementation"),
            Self::Verification => write!(f, "verification"),
        }
    }
}

// ─── CoordinatorState ──────────────────────────────────────────────────────

/// Errors from coordinator operations.
#[derive(Debug, Error)]
pub enum CoordinatorError {
    /// Coordinator is already in the final phase.
    #[error("coordinator already in final phase")]
    AlreadyComplete,
    /// Phase name provided is invalid.
    #[error("invalid phase name: {0}")]
    InvalidPhase(String),
}

/// State machine for the coordinator's four-phase workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorState {
    /// Current phase in the workflow.
    pub current_phase: CoordinatorPhase,
    /// Whether the coordinator has completed all phases.
    pub is_complete: bool,
    /// Phase summaries collected during execution.
    pub phase_summaries: Vec<(CoordinatorPhase, String)>,
    /// Whether the coordinator is actively running.
    pub is_running: bool,
}

impl Default for CoordinatorState {
    fn default() -> Self {
        Self::new()
    }
}

impl CoordinatorState {
    /// Create a new coordinator starting at the Research phase.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current_phase: CoordinatorPhase::Research,
            is_complete: false,
            phase_summaries: Vec::new(),
            is_running: false,
        }
    }

    /// Start the coordinator workflow.
    pub fn start(&mut self) {
        self.is_running = true;
    }

    /// Advance to the next phase with a summary of the current phase.
    ///
    /// Records the current phase's summary before advancing.
    /// Returns `Ok` with the new phase, or `Err` if already complete.
    pub fn advance(&mut self, summary: &str) -> Result<CoordinatorPhase, CoordinatorError> {
        // Save the current phase summary
        self.phase_summaries
            .push((self.current_phase, summary.to_string()));

        match self.current_phase.next() {
            Some(next_phase) => {
                self.current_phase = next_phase;
                Ok(next_phase)
            }
            None => {
                self.is_complete = true;
                Err(CoordinatorError::AlreadyComplete)
            }
        }
    }

    /// Manually set the current phase (useful for recovery/jump).
    pub fn set_phase(&mut self, phase: CoordinatorPhase) {
        self.current_phase = phase;
    }

    /// Mark the coordinator as stopped (not completed).
    pub fn stop(&mut self) {
        self.is_running = false;
    }

    /// Get the summary of a specific phase, if recorded.
    #[must_use]
    pub fn get_phase_summary(&self, phase: CoordinatorPhase) -> Option<&str> {
        self.phase_summaries
            .iter()
            .find(|(p, _)| *p == phase)
            .map(|(_, s)| s.as_str())
    }

    /// Get all recorded phase summaries.
    #[must_use]
    pub fn all_summaries(&self) -> &[(CoordinatorPhase, String)] {
        &self.phase_summaries
    }

    /// Get the number of phases completed so far.
    #[must_use]
    pub fn phases_completed(&self) -> usize {
        self.phase_summaries.len()
    }
}

// ─── Coordinator Tool Whitelist ────────────────────────────────────────────

/// Tool names allowed in CoordinatorMode.
///
/// The coordinator only has access to tools for research, planning,
/// and agent dispatch — not direct file mutation.
#[must_use]
pub fn coordinator_tool_whitelist() -> Vec<&'static str> {
    vec![
        // Read-only tools for research
        "read",
        "glob",
        "grep",
        "web_search",
        "web_fetch",
        "tool_search",
        // Planning and communication
        "agent",
        "send_message",
        "brief",
        "ask_user_question",
        // Task management
        "task_create",
        "task_get",
        "task_list",
        "task_update",
        // Coordination
        "enter_plan_mode",
        "exit_plan_mode",
        "todo_write",
    ]
}

/// Check if a tool name is available in CoordinatorMode.
#[must_use]
pub fn is_coordinator_tool(tool_name: &str) -> bool {
    coordinator_tool_whitelist().iter().any(|&t| t == tool_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── CoordinatorPhase tests ─────────────────────────────────────────────

    #[test]
    fn phase_sequence() {
        assert_eq!(
            CoordinatorPhase::Research.next(),
            Some(CoordinatorPhase::Synthesis)
        );
        assert_eq!(
            CoordinatorPhase::Synthesis.next(),
            Some(CoordinatorPhase::Implementation)
        );
        assert_eq!(
            CoordinatorPhase::Implementation.next(),
            Some(CoordinatorPhase::Verification)
        );
        assert_eq!(CoordinatorPhase::Verification.next(), None);
    }

    #[test]
    fn phase_is_first_and_final() {
        assert!(CoordinatorPhase::Research.is_first());
        assert!(!CoordinatorPhase::Synthesis.is_first());
        assert!(CoordinatorPhase::Verification.is_final());
        assert!(!CoordinatorPhase::Research.is_final());
    }

    #[test]
    fn phase_display() {
        assert_eq!(CoordinatorPhase::Research.to_string(), "research");
        assert_eq!(CoordinatorPhase::Synthesis.to_string(), "synthesis");
        assert_eq!(
            CoordinatorPhase::Implementation.to_string(),
            "implementation"
        );
        assert_eq!(CoordinatorPhase::Verification.to_string(), "verification");
    }

    // ─── CoordinatorState tests ────────────────────────────────────────────

    #[test]
    fn coordinator_starts_at_research() {
        let state = CoordinatorState::new();
        assert_eq!(state.current_phase, CoordinatorPhase::Research);
        assert!(!state.is_complete);
        assert!(!state.is_running);
        assert!(state.phase_summaries.is_empty());
    }

    #[test]
    fn coordinator_advance_through_all_phases() {
        let mut state = CoordinatorState::new();
        state.start();

        let p = state.advance("gathered info").unwrap();
        assert_eq!(p, CoordinatorPhase::Synthesis);
        assert_eq!(state.phases_completed(), 1);

        let p = state.advance("designed plan").unwrap();
        assert_eq!(p, CoordinatorPhase::Implementation);
        assert_eq!(state.phases_completed(), 2);

        let p = state.advance("built feature").unwrap();
        assert_eq!(p, CoordinatorPhase::Verification);
        assert_eq!(state.phases_completed(), 3);

        let result = state.advance("verified");
        assert!(matches!(result, Err(CoordinatorError::AlreadyComplete)));
        assert!(state.is_complete);
        assert_eq!(state.phases_completed(), 4);
    }

    #[test]
    fn coordinator_records_summaries() {
        let mut state = CoordinatorState::new();
        state.advance("summary A").unwrap();
        state.advance("summary B").unwrap();

        assert_eq!(
            state.get_phase_summary(CoordinatorPhase::Research),
            Some("summary A")
        );
        assert_eq!(
            state.get_phase_summary(CoordinatorPhase::Synthesis),
            Some("summary B")
        );
    }

    #[test]
    fn coordinator_set_phase() {
        let mut state = CoordinatorState::new();
        state.set_phase(CoordinatorPhase::Implementation);
        assert_eq!(state.current_phase, CoordinatorPhase::Implementation);
    }

    #[test]
    fn coordinator_stop() {
        let mut state = CoordinatorState::new();
        state.start();
        assert!(state.is_running);
        state.stop();
        assert!(!state.is_running);
    }

    #[test]
    fn coordinator_all_summaries() {
        let mut state = CoordinatorState::new();
        state.advance("A").unwrap();
        state.advance("B").unwrap();

        let summaries = state.all_summaries();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].1, "A");
        assert_eq!(summaries[1].1, "B");
    }

    #[test]
    fn coordinator_serialization() {
        let mut state = CoordinatorState::new();
        state.advance("research done").unwrap();

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("synthesis"));
        assert!(json.contains("research done"));

        let decoded: CoordinatorState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.current_phase, CoordinatorPhase::Synthesis);
    }

    // ─── Tool whitelist tests ──────────────────────────────────────────────

    #[test]
    fn coordinator_whitelist_contains_expected_tools() {
        let tools = coordinator_tool_whitelist();
        assert!(tools.contains(&"read"));
        assert!(tools.contains(&"agent"));
        assert!(tools.contains(&"send_message"));
        assert!(tools.contains(&"task_list"));
        assert!(tools.contains(&"enter_plan_mode"));
    }

    #[test]
    fn is_coordinator_tool_checks() {
        assert!(is_coordinator_tool("read"));
        assert!(is_coordinator_tool("agent"));
        assert!(is_coordinator_tool("todo_write"));
        assert!(!is_coordinator_tool("bash"));
        assert!(!is_coordinator_tool("write"));
        assert!(!is_coordinator_tool("edit"));
    }

    #[test]
    fn phases_completed_count() {
        let mut state = CoordinatorState::new();
        assert_eq!(state.phases_completed(), 0);
        state.advance("a").unwrap();
        assert_eq!(state.phases_completed(), 1);
        state.advance("b").unwrap();
        assert_eq!(state.phases_completed(), 2);
    }
}
