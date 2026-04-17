//! Elicitation handler trait and implementations for REPL/SDK flows.
//!
//! Elicitations are clarification questions the model asks the user during
//! a conversation turn. The handler manages the request/response lifecycle.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// An elicitation option presented to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationOption {
    /// Display label for the option.
    pub label: String,
    /// Description of what this option means.
    pub description: String,
}

/// A single elicitation question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationQuestion {
    /// The question to ask the user.
    pub question: String,
    /// Short header label (max 12 chars).
    pub header: String,
    /// Available options for the user to choose from.
    pub options: Vec<ElicitationOption>,
    /// Whether the user can select multiple options.
    pub multi_select: bool,
}

/// The user's response to an elicitation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    /// The elicitation request ID.
    pub request_id: String,
    /// Selected option labels (single or multi).
    pub selected: Vec<String>,
    /// Optional free-text input from the user ("Other").
    pub other_text: Option<String>,
}

/// Result of an elicitation request.
#[derive(Debug, Clone)]
pub enum ElicitationResult {
    /// The user responded with a selection.
    Responded(ElicitationResponse),
    /// The user cancelled the elicitation.
    Cancelled,
    /// The elicitation timed out.
    TimedOut,
}

/// Trait for handling elicitation requests.
///
/// Elicitations are how the model asks the user clarifying questions
/// during a turn. Different implementations handle REPL (interactive
/// terminal) vs SDK (structured I/O) vs headless modes.
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    /// Request an elicitation from the user.
    ///
    /// Returns the user's response, cancellation, or timeout.
    async fn request(
        &self,
        questions: Vec<ElicitationQuestion>,
    ) -> ElicitationResult;
}

/// REPL elicitation handler — prompts interactively via the terminal.
///
/// In a real implementation, this would use a TUI library or readline
/// to display options and collect user input. For now, this is a
/// placeholder that logs the elicitation and returns Cancelled.
#[derive(Debug, Default)]
pub struct ReplElicitationHandler;

#[async_trait]
impl ElicitationHandler for ReplElicitationHandler {
    async fn request(
        &self,
        questions: Vec<ElicitationQuestion>,
    ) -> ElicitationResult {
        for q in &questions {
            tracing::info!(
                "ELICITATION [{header}]: {question}",
                header = q.header,
                question = q.question,
            );
            for opt in &q.options {
                tracing::info!("  - {label}: {desc}", label = opt.label, desc = opt.description);
            }
        }
        // In production, this would display the UI and wait for input.
        // For now, return cancelled (headless mode).
        ElicitationResult::Cancelled
    }
}

/// SDK elicitation handler — sends structured I/O to the SDK consumer.
///
/// This handler serializes elicitation questions into a structured format
/// suitable for SDK consumers (e.g., JSON-RPC notifications).
#[derive(Debug, Default)]
pub struct SdkElicitationHandler;

#[async_trait]
impl ElicitationHandler for SdkElicitationHandler {
    async fn request(
        &self,
        questions: Vec<ElicitationQuestion>,
    ) -> ElicitationResult {
        // Emit structured elicitation event
        for q in &questions {
            let event_json = serde_json::json!({
                "type": "elicitation_request",
                "question": q.question,
                "header": q.header,
                "options": q.options,
                "multi_select": q.multi_select,
            });
            tracing::info!(
                "SDK ELICITATION: {json}",
                json = event_json
            );
        }
        // In production, this would send the elicitation to the SDK
        // and await a response. For now, return cancelled.
        ElicitationResult::Cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_question() -> ElicitationQuestion {
        ElicitationQuestion {
            question: "Which approach should we use?".into(),
            header: "Approach".into(),
            options: vec![
                ElicitationOption {
                    label: "Simple".into(),
                    description: "Use the simplest approach".into(),
                },
                ElicitationOption {
                    label: "Robust".into(),
                    description: "Use a more robust but complex approach".into(),
                },
            ],
            multi_select: false,
        }
    }

    #[test]
    fn elicitation_question_serialization() {
        let q = make_question();
        let json = serde_json::to_string(&q).unwrap();
        assert!(json.contains("Which approach"));
        assert!(json.contains("Simple"));
    }

    #[test]
    fn elicitation_response_serialization() {
        let r = ElicitationResponse {
            request_id: "elic_1".into(),
            selected: vec!["Simple".into()],
            other_text: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("elic_1"));
        assert!(json.contains("Simple"));
    }

    #[tokio::test]
    async fn repl_handler_logs_and_returns_cancelled() {
        let handler = ReplElicitationHandler;
        let result = handler.request(vec![make_question()]).await;
        assert!(matches!(result, ElicitationResult::Cancelled));
    }

    #[tokio::test]
    async fn sdk_handler_returns_cancelled() {
        let handler = SdkElicitationHandler;
        let result = handler.request(vec![make_question()]).await;
        assert!(matches!(result, ElicitationResult::Cancelled));
    }
}
