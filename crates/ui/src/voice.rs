//! Voice STT — speech-to-text input state machine.
//!
//! Manages the lifecycle of voice recording → transcription → result.
//! Feature-gated behind `voice`.

/// Voice recording state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceState {
    /// Not recording.
    Idle,
    /// Actively listening for audio input.
    Listening,
    /// Audio captured, waiting for transcription result.
    Transcribing,
    /// Transcription failed or error occurred.
    Error(String),
}

impl VoiceState {
    /// Create a new idle voice state.
    #[must_use]
    pub fn new() -> Self {
        Self::Idle
    }

    /// Start listening.
    ///
    /// A state in `Error` is not a state to start from: recovering from a
    /// failure is [`VoiceState::retry`], so `start_listening` on an error
    /// leaves it as it is rather than clearing the reason the user needs to
    /// see. It used to compare against `Error(String::new())`, which never
    /// matched a real error and so silently swallowed it.
    pub fn start_listening(&mut self) {
        if *self == Self::Idle {
            *self = Self::Listening;
        }
    }

    /// Receive a transcription result.
    pub fn receive_transcription(&mut self, text: String) {
        if *self == Self::Transcribing {
            if text.is_empty() {
                *self = Self::Listening;
            } else {
                *self = Self::Idle;
            }
        }
    }

    /// Record a transcription result (consumes Transcribing state).
    pub fn finish_transcribing(&mut self, text: String) -> Option<String> {
        if *self == Self::Transcribing {
            if text.is_empty() {
                *self = Self::Listening;
                None
            } else {
                let result = Some(text);
                *self = Self::Idle;
                result
            }
        } else {
            None
        }
    }

    /// Transition to transcribing (audio captured, processing).
    pub fn begin_transcribing(&mut self) {
        if *self == Self::Listening {
            *self = Self::Transcribing;
        }
    }

    /// Record an error.
    pub fn fail(&mut self, error: String) {
        *self = Self::Error(error);
    }

    /// Retry after error (go back to idle).
    pub fn retry(&mut self) {
        if matches!(self, Self::Error(_)) {
            *self = Self::Idle;
        }
    }

    /// Check if voice input is currently active (not idle).
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// Human-readable status label for the UI.
    #[must_use]
    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Idle => "Voice",
            Self::Listening => "Listening...",
            Self::Transcribing => "Transcribing...",
            Self::Error(_) => "Voice Error",
        }
    }
}

impl Default for VoiceState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_voice_cycle() {
        let mut state = VoiceState::new();
        assert_eq!(state, VoiceState::Idle);
        assert!(!state.is_active());

        state.start_listening();
        assert_eq!(state, VoiceState::Listening);
        assert!(state.is_active());
        assert_eq!(state.status_label(), "Listening...");

        state.begin_transcribing();
        assert_eq!(state, VoiceState::Transcribing);
        assert_eq!(state.status_label(), "Transcribing...");

        let result = state.finish_transcribing("hello world".into());
        assert_eq!(result, Some("hello world".into()));
        assert_eq!(state, VoiceState::Idle);
        assert!(!state.is_active());
    }

    #[test]
    fn error_state_and_recovery() {
        let mut state = VoiceState::new();
        state.start_listening();
        state.begin_transcribing();
        state.fail("microphone unavailable".into());
        assert!(matches!(state, VoiceState::Error(_)));
        assert_eq!(state.status_label(), "Voice Error");
        assert!(state.is_active());

        state.retry();
        assert_eq!(state, VoiceState::Idle);
    }

    #[test]
    fn empty_transcription_returns_to_listening() {
        let mut state = VoiceState::new();
        state.start_listening();
        state.begin_transcribing();
        let result = state.finish_transcribing(String::new());
        assert!(result.is_none());
        assert_eq!(state, VoiceState::Listening);
    }

    #[test]
    fn start_listening_does_not_swallow_an_error() {
        // An error is recovered from with `retry`, never by starting over:
        // clearing it here hid the failure's reason from the status line.
        let mut state = VoiceState::new();
        state.fail("microphone unavailable".into());
        state.start_listening();
        assert!(
            matches!(&state, VoiceState::Error(reason) if reason == "microphone unavailable"),
            "start_listening cleared the error: {state:?}"
        );
        state.retry();
        state.start_listening();
        assert_eq!(state, VoiceState::Listening);
    }

    #[test]
    fn consecutive_voice_inputs() {
        let mut state = VoiceState::new();
        for text in ["hi", "search for tests", "run cargo test"] {
            state.start_listening();
            state.begin_transcribing();
            let result = state.finish_transcribing(text.into());
            assert_eq!(result, Some(text.into()));
            assert_eq!(state, VoiceState::Idle);
        }
    }
}
