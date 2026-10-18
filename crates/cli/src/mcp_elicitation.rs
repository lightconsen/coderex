//! Bridges MCP server elicitation requests to the session's user interface.
//!
//! MCP servers may ask the person a question mid-request (`elicitation/create`)
//! and block until the answer arrives. The TUI already has a path for exactly
//! that shape of question — a request sent to the event loop, an answer read
//! from the input line — so this module reuses it rather than adding a second
//! way to prompt. A non-interactive session has nobody to ask, and says so
//! instead of leaving the server waiting.

use coderex_core::elicitation::{
    ElicitationHandler, ElicitationQuestion, ElicitationResponse, ElicitationResult,
    ReplElicitationHandler,
};
use coderex_mcp::channel::InboundRequestHandler;
use coderex_mcp::elicitation::McpElicitationHandler;
use std::sync::{Arc, Mutex};

/// A question the TUI event loop should put in front of the user.
#[derive(Debug)]
pub struct TuiElicitationRequest {
    /// The question text, already shown in the transcript by the caller.
    pub prompt: String,
    /// Channel for the user's answer (`{"answer": "..."}`).
    pub response_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// The session's answer to server-initiated elicitation requests.
///
/// The TUI registers a sender at startup; until then — and in a session with
/// no TUI — an interactive session falls back to prompting on the terminal,
/// and a non-interactive one declines.
#[derive(Debug)]
pub struct SessionElicitationHandler {
    /// Where to send a question when the full-screen TUI is running.
    tui: Mutex<Option<tokio::sync::mpsc::Sender<TuiElicitationRequest>>>,
    /// Whether this session has a person at a terminal at all.
    interactive: bool,
}

impl SessionElicitationHandler {
    /// Create the session handler.
    ///
    /// `interactive` is false for `--print`, a piped prompt, and any session
    /// whose stdin is not a terminal: those answer `cancel` rather than read
    /// from a stream that is not a keyboard.
    #[must_use]
    pub fn new(interactive: bool) -> Arc<Self> {
        Arc::new(Self {
            tui: Mutex::new(None),
            interactive,
        })
    }

    /// Route questions into the running TUI.
    pub fn set_tui_sender(&self, tx: tokio::sync::mpsc::Sender<TuiElicitationRequest>) {
        if let Ok(mut slot) = self.tui.lock() {
            *slot = Some(tx);
        }
    }

    /// Stop routing questions into the TUI (it is shutting down).
    pub fn clear_tui_sender(&self) {
        if let Ok(mut slot) = self.tui.lock() {
            *slot = None;
        }
    }

    /// The handler to install on the MCP manager.
    ///
    /// This is the translation layer: it turns `elicitation/create` into the
    /// engine's question shape and back into an `action`/`content` result.
    pub fn inbound(self: &Arc<Self>) -> Arc<dyn InboundRequestHandler> {
        let prompt: Arc<dyn ElicitationHandler> = self.clone();
        Arc::new(McpElicitationHandler::new(prompt))
    }
}

#[async_trait::async_trait]
impl ElicitationHandler for SessionElicitationHandler {
    async fn request(&self, questions: Vec<ElicitationQuestion>) -> ElicitationResult {
        let Some(question) = questions.first() else {
            return ElicitationResult::Cancelled;
        };

        // Clone the sender out of the lock, then release it: the answer may
        // take as long as the person does.
        let sender = self.tui.lock().ok().and_then(|slot| slot.as_ref().cloned());

        if let Some(tx) = sender {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let request = TuiElicitationRequest {
                prompt: question.question.clone(),
                response_tx,
            };
            if tx.send(request).await.is_err() {
                return ElicitationResult::Cancelled;
            }
            return match response_rx.await {
                Ok(value) => answer_from(value, question),
                Err(_) => ElicitationResult::Cancelled,
            };
        }

        if self.interactive {
            return ReplElicitationHandler::default().request(questions).await;
        }

        tracing::warn!(
            message = %question.question,
            "an MCP server asked for user input, but this session is non-interactive; answering cancel"
        );
        ElicitationResult::Cancelled
    }
}

/// Turn the TUI's `{"answer": "..."}` into the engine's response type.
fn answer_from(value: serde_json::Value, question: &ElicitationQuestion) -> ElicitationResult {
    value
        .get("answer")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or(ElicitationResult::Cancelled, |text| {
            ElicitationResult::Responded(ElicitationResponse {
                request_id: question.header.clone(),
                selected: Vec::new(),
                other_text: Some(text.to_string()),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> ElicitationQuestion {
        ElicitationQuestion {
            question: "Which environment?".into(),
            header: "Environment".into(),
            options: Vec::new(),
            multi_select: false,
        }
    }

    /// A headless session must answer rather than wait for input that is not
    /// coming. Reading the process's stdin here would make the test depend on
    /// how the harness launched it.
    #[tokio::test]
    async fn a_non_interactive_session_cancels_instead_of_reading_stdin() {
        let handler = SessionElicitationHandler::new(false);
        let result = handler.request(vec![question()]).await;
        assert!(matches!(result, ElicitationResult::Cancelled));
    }

    #[tokio::test]
    async fn a_tui_sender_receives_the_question_and_its_answer_returns() {
        let handler = SessionElicitationHandler::new(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        handler.set_tui_sender(tx);

        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("the question reached the TUI");
            assert_eq!(request.prompt, "Which environment?");
            let _ = request
                .response_tx
                .send(serde_json::json!({"answer": "staging"}));
        });

        let result = handler.request(vec![question()]).await;
        responder.await.unwrap();

        match result {
            ElicitationResult::Responded(response) => {
                assert_eq!(response.other_text.as_deref(), Some("staging"));
            }
            other => panic!("expected the TUI answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_answer_is_a_cancel() {
        let handler = SessionElicitationHandler::new(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        handler.set_tui_sender(tx);

        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("the question reached the TUI");
            let _ = request
                .response_tx
                .send(serde_json::json!({"answer": "  "}));
        });

        let result = handler.request(vec![question()]).await;
        responder.await.unwrap();
        assert!(matches!(result, ElicitationResult::Cancelled));
    }

    /// The handler the manager receives really is the MCP translation layer.
    #[tokio::test]
    async fn the_mcp_handler_answers_an_elicitation_create() {
        let handler = SessionElicitationHandler::new(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        handler.set_tui_sender(tx);

        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("the question reached the TUI");
            let _ = request
                .response_tx
                .send(serde_json::json!({"answer": "staging"}));
        });

        let answer = handler
            .inbound()
            .handle(
                coderex_mcp::elicitation::ELICITATION_CREATE,
                Some(&serde_json::json!({
                    "message": "Which environment?",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {"environment": {"type": "string"}}
                    }
                })),
            )
            .await
            .expect("the session answered");

        responder.await.unwrap();
        assert_eq!(answer["action"], "accept");
        assert_eq!(answer["content"]["environment"], "staging");
    }
}
