//! Bridge session state machine with validated transitions.

use std::fmt;

/// State of a bridged session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeSessionState {
    /// Session has been assigned to a worker.
    Assigned,
    /// Worker is preparing the environment.
    Preparing,
    /// Creating a git worktree for the session.
    CreatingWorktree,
    /// Spawning the CodeRex process.
    Spawning,
    /// Session is actively running.
    Running,
    /// Waiting for user permission on a tool call.
    AwaitingPermission,
    /// Archiving session results.
    Archiving,
    /// Cleaning up session resources.
    CleaningUp,
    /// Session completed successfully.
    Completed,
    /// Session failed with an error message.
    Failed(String),
}

impl BridgeSessionState {
    /// Check if a transition from `self` to `target` is valid.
    #[must_use]
    pub const fn can_transition_to(&self, target: &Self) -> bool {
        matches!(
            (self, target),
            // Assigned → Preparing | Failed
            (Self::Assigned, Self::Preparing | Self::Failed(_))
                | (
                    Self::Preparing,
                    Self::CreatingWorktree | Self::Spawning | Self::Failed(_)
                )
                | (Self::CreatingWorktree, Self::Spawning | Self::Failed(_))
                | (Self::Spawning | Self::AwaitingPermission, Self::Running)
                | (
                    Self::Spawning
                        | Self::Running
                        | Self::AwaitingPermission
                        | Self::Archiving
                        | Self::CleaningUp,
                    Self::Failed(_)
                )
                | (Self::Running, Self::AwaitingPermission | Self::Archiving)
                | (Self::Archiving, Self::CleaningUp)
                | (Self::CleaningUp, Self::Completed)
        )
    }

    /// Check if this is a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_))
    }

    /// Human-readable label.
    #[must_use]
    pub const fn label(&self) -> &str {
        match self {
            Self::Assigned => "assigned",
            Self::Preparing => "preparing",
            Self::CreatingWorktree => "creating_worktree",
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::AwaitingPermission => "awaiting_permission",
            Self::Archiving => "archiving",
            Self::CleaningUp => "cleaning_up",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
        }
    }
}

impl fmt::Display for BridgeSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// A state machine for a bridge session.
#[derive(Debug, Clone)]
pub struct BridgeSessionStateMachine {
    /// Session ID.
    pub session_id: String,
    /// Current state.
    pub current: BridgeSessionState,
    /// History of states (for debugging/audit).
    pub history: Vec<BridgeSessionState>,
}

impl BridgeSessionStateMachine {
    /// Create a new state machine starting at Assigned.
    #[must_use]
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            current: BridgeSessionState::Assigned,
            history: vec![BridgeSessionState::Assigned],
        }
    }

    /// Attempt to transition to a new state.
    pub fn transition(&mut self, target: BridgeSessionState) -> Result<(), String> {
        if self.current.is_terminal() {
            return Err(format!(
                "cannot transition from terminal state '{}'",
                self.current.label()
            ));
        }

        if !self.current.can_transition_to(&target) {
            return Err(format!(
                "invalid transition: '{}' → '{}'",
                self.current.label(),
                target.label()
            ));
        }

        self.current = target.clone();
        self.history.push(target);
        Ok(())
    }

    /// Transition to a failed state with an error message (from any non-terminal state).
    pub fn fail(&mut self, error: String) {
        self.current = BridgeSessionState::Failed(error);
        self.history.push(self.current.clone());
    }

    /// Check if the session is in a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.current.is_terminal()
    }

    /// Get the number of state transitions so far.
    #[must_use]
    pub fn transition_count(&self) -> usize {
        self.history.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigned_to_preparing() {
        assert!(BridgeSessionState::Assigned.can_transition_to(&BridgeSessionState::Preparing));
    }

    #[test]
    fn preparing_to_spawning_or_worktree() {
        assert!(
            BridgeSessionState::Preparing.can_transition_to(&BridgeSessionState::CreatingWorktree)
        );
        assert!(BridgeSessionState::Preparing.can_transition_to(&BridgeSessionState::Spawning));
    }

    #[test]
    fn worktree_to_spawning() {
        assert!(
            BridgeSessionState::CreatingWorktree.can_transition_to(&BridgeSessionState::Spawning)
        );
    }

    #[test]
    fn spawning_to_running_or_failed() {
        assert!(BridgeSessionState::Spawning.can_transition_to(&BridgeSessionState::Running));
        assert!(
            BridgeSessionState::Spawning
                .can_transition_to(&BridgeSessionState::Failed("oops".into()))
        );
    }

    #[test]
    fn running_to_various() {
        assert!(
            BridgeSessionState::Running.can_transition_to(&BridgeSessionState::AwaitingPermission)
        );
        assert!(BridgeSessionState::Running.can_transition_to(&BridgeSessionState::Archiving));
        assert!(
            BridgeSessionState::Running
                .can_transition_to(&BridgeSessionState::Failed("error".into()))
        );
    }

    #[test]
    fn awaiting_permission_back_to_running() {
        assert!(
            BridgeSessionState::AwaitingPermission.can_transition_to(&BridgeSessionState::Running)
        );
        assert!(
            BridgeSessionState::AwaitingPermission
                .can_transition_to(&BridgeSessionState::Failed("denied".into()))
        );
    }

    #[test]
    fn archiving_to_cleaning_up() {
        assert!(BridgeSessionState::Archiving.can_transition_to(&BridgeSessionState::CleaningUp));
        assert!(
            BridgeSessionState::Archiving
                .can_transition_to(&BridgeSessionState::Failed("archive error".into()))
        );
    }

    #[test]
    fn cleaning_up_to_completed_or_failed() {
        assert!(BridgeSessionState::CleaningUp.can_transition_to(&BridgeSessionState::Completed));
        assert!(
            BridgeSessionState::CleaningUp
                .can_transition_to(&BridgeSessionState::Failed("cleanup error".into()))
        );
    }

    #[test]
    fn invalid_transitions() {
        assert!(!BridgeSessionState::Assigned.can_transition_to(&BridgeSessionState::Running));
        assert!(!BridgeSessionState::Completed.can_transition_to(&BridgeSessionState::Assigned));
        assert!(!BridgeSessionState::Running.can_transition_to(&BridgeSessionState::Assigned));
        assert!(!BridgeSessionState::Spawning.can_transition_to(&BridgeSessionState::Completed));
    }

    #[test]
    fn terminal_states() {
        assert!(BridgeSessionState::Completed.is_terminal());
        assert!(BridgeSessionState::Failed("err".into()).is_terminal());
        assert!(!BridgeSessionState::Running.is_terminal());
        assert!(!BridgeSessionState::Assigned.is_terminal());
    }

    #[test]
    fn state_machine_full_lifecycle() {
        let mut sm = BridgeSessionStateMachine::new("s1".into());
        assert!(matches!(sm.current, BridgeSessionState::Assigned));

        sm.transition(BridgeSessionState::Preparing).unwrap();
        sm.transition(BridgeSessionState::CreatingWorktree).unwrap();
        sm.transition(BridgeSessionState::Spawning).unwrap();
        sm.transition(BridgeSessionState::Running).unwrap();
        sm.transition(BridgeSessionState::Archiving).unwrap();
        sm.transition(BridgeSessionState::CleaningUp).unwrap();
        sm.transition(BridgeSessionState::Completed).unwrap();

        assert!(sm.is_terminal());
        assert_eq!(sm.transition_count(), 8);
    }

    #[test]
    fn state_machine_fail_at_various_points() {
        let mut sm = BridgeSessionStateMachine::new("s2".into());
        sm.transition(BridgeSessionState::Preparing).unwrap();
        sm.fail("worktree failed".into());
        assert!(sm.is_terminal());
        assert!(matches!(sm.current, BridgeSessionState::Failed(_)));
    }

    #[test]
    fn state_machine_terminal_transition_blocked() {
        let mut sm = BridgeSessionStateMachine::new("s3".into());
        sm.transition(BridgeSessionState::Preparing).unwrap();
        sm.transition(BridgeSessionState::Spawning).unwrap();
        sm.transition(BridgeSessionState::Running).unwrap();
        sm.transition(BridgeSessionState::Archiving).unwrap();
        sm.transition(BridgeSessionState::CleaningUp).unwrap();
        sm.transition(BridgeSessionState::Completed).unwrap();

        let result = sm.transition(BridgeSessionState::Assigned);
        assert!(result.is_err());
    }

    #[test]
    fn early_states_can_fail_through_the_validated_api() {
        for target in [
            BridgeSessionState::Assigned,
            BridgeSessionState::Preparing,
            BridgeSessionState::CreatingWorktree,
        ] {
            let mut sm = BridgeSessionStateMachine::new("s-fail".into());
            match target {
                BridgeSessionState::Preparing => {
                    sm.transition(BridgeSessionState::Preparing).unwrap();
                }
                BridgeSessionState::CreatingWorktree => {
                    sm.transition(BridgeSessionState::Preparing).unwrap();
                    sm.transition(BridgeSessionState::CreatingWorktree).unwrap();
                }
                _ => {}
            }
            sm.transition(BridgeSessionState::Failed("spawn failed".into()))
                .unwrap();
            assert!(sm.is_terminal());
        }
    }

    #[test]
    fn state_machine_full_lifecycle_with_permission_wait() {
        let mut sm = BridgeSessionStateMachine::new("s4".into());

        sm.transition(BridgeSessionState::Preparing).unwrap();
        sm.transition(BridgeSessionState::CreatingWorktree).unwrap();
        sm.transition(BridgeSessionState::Spawning).unwrap();
        sm.transition(BridgeSessionState::Running).unwrap();
        sm.transition(BridgeSessionState::AwaitingPermission)
            .unwrap();
        sm.transition(BridgeSessionState::Running).unwrap();
        sm.transition(BridgeSessionState::Archiving).unwrap();
        sm.transition(BridgeSessionState::CleaningUp).unwrap();
        sm.transition(BridgeSessionState::Completed).unwrap();

        assert_eq!(
            sm.history,
            vec![
                BridgeSessionState::Assigned,
                BridgeSessionState::Preparing,
                BridgeSessionState::CreatingWorktree,
                BridgeSessionState::Spawning,
                BridgeSessionState::Running,
                BridgeSessionState::AwaitingPermission,
                BridgeSessionState::Running,
                BridgeSessionState::Archiving,
                BridgeSessionState::CleaningUp,
                BridgeSessionState::Completed,
            ]
        );
        assert!(sm.is_terminal());
    }

    #[test]
    fn permission_and_terminal_moves_stay_refused() {
        // Archiving and cleaning up are the only routes into the terminal
        // states; a running session must not jump straight there.
        assert!(!BridgeSessionState::Running.can_transition_to(&BridgeSessionState::Completed));
        assert!(!BridgeSessionState::Archiving.can_transition_to(&BridgeSessionState::Completed));
        assert!(!BridgeSessionState::Spawning.can_transition_to(&BridgeSessionState::Completed));
        // Only a running session can start waiting for permission.
        assert!(
            !BridgeSessionState::Preparing
                .can_transition_to(&BridgeSessionState::AwaitingPermission)
        );
        assert!(
            !BridgeSessionState::AwaitingPermission
                .can_transition_to(&BridgeSessionState::Archiving)
        );
    }

    #[test]
    fn state_labels() {
        assert_eq!(BridgeSessionState::Assigned.label(), "assigned");
        assert_eq!(BridgeSessionState::Failed("x".into()).label(), "failed");
        assert_eq!(BridgeSessionState::Running.to_string(), "running");
    }
}
