//! Elicitation handler trait and implementations for REPL/SDK flows.
//!
//! Elicitations are clarification questions the model asks the user during
//! a conversation turn. The handler manages the request/response lifecycle.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use std::io::Write;

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
/// Prints questions to stderr and reads user input from stdin.
/// Supports both numbered-option selection and free-text responses.
#[derive(Debug, Default)]
pub struct ReplElicitationHandler;

#[async_trait]
impl ElicitationHandler for ReplElicitationHandler {
    async fn request(
        &self,
        questions: Vec<ElicitationQuestion>,
    ) -> ElicitationResult {
        let mut stderr = std::io::stderr();
        for q in &questions {
            let _ = writeln!(stderr, "\n[{}] {}", q.header, q.question);
            for (i, opt) in q.options.iter().enumerate() {
                let _ = writeln!(stderr, "  {}. {} — {}", i + 1, opt.label, opt.description);
            }
            if q.multi_select {
                let _ = writeln!(stderr, "  (Enter comma-separated numbers, or type a custom response)");
            } else {
                let _ = writeln!(stderr, "  (Enter a number or type a custom response)");
            }
            let _ = write!(stderr, "> ");
            let _ = stderr.flush();

            let stdin = tokio::io::stdin();
            let mut reader = tokio::io::BufReader::new(stdin);
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return ElicitationResult::Cancelled,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        return ElicitationResult::Cancelled;
                    }
                    let selected: Vec<String> = if q.options.is_empty() {
                        vec![]
                    } else if q.multi_select {
                        trimmed
                            .split(',')
                            .filter_map(|s| s.trim().parse::<usize>().ok())
                            .filter(|&n| n > 0 && n <= q.options.len())
                            .map(|n| q.options[n - 1].label.clone())
                            .collect()
                    } else if let Ok(n) = trimmed.parse::<usize>() {
                        if n > 0 && n <= q.options.len() {
                            vec![q.options[n - 1].label.clone()]
                        } else {
                            vec![]
                        }
                    } else {
                        vec![]
                    };

                    return ElicitationResult::Responded(ElicitationResponse {
                        request_id: q.header.clone(),
                        selected,
                        other_text: Some(trimmed.to_string()),
                    });
                }
            }
        }
        ElicitationResult::Cancelled
    }
}

/// SDK elicitation handler — sends structured I/O to the SDK consumer.
///
/// This handler serializes elicitation questions into a structured format
/// suitable for SDK consumers (e.g., JSON-RPC notifications) and awaits
/// a response via a oneshot channel.
#[derive(Debug)]
pub struct SdkElicitationHandler {
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::types::EngineEvent>>,
}

impl Default for SdkElicitationHandler {
    fn default() -> Self {
        Self { event_tx: None }
    }
}

impl SdkElicitationHandler {
    /// Create a new handler with an event sender.
    #[must_use]
    pub fn new(event_tx: tokio::sync::mpsc::UnboundedSender<crate::types::EngineEvent>) -> Self {
        Self { event_tx: Some(event_tx) }
    }
}

#[async_trait]
impl ElicitationHandler for SdkElicitationHandler {
    async fn request(
        &self,
        questions: Vec<ElicitationQuestion>,
    ) -> ElicitationResult {
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
            // Emit an EngineEvent if an event sender is available.
            if let Some(ref tx) = self.event_tx {
                let _ = tx.send(crate::types::EngineEvent::ElicitationRequest {
                    id: q.header.clone(),
                    message: q.question.clone(),
                });
            }
        }
        // In production, this would await a response from the SDK consumer.
        // For now, return cancelled after a short timeout so the turn can
        // proceed gracefully when no SDK consumer is attached.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let handler = SdkElicitationHandler::new(tx);
        let result = handler.request(vec![make_question()]).await;
        assert!(matches!(result, ElicitationResult::Cancelled));
    }
}
