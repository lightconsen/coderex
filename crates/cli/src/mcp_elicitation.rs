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
use coderex_mcp::elicitation::{
    FormElicitationHandler, FormField, FormResult, FormSpec, McpElicitationHandler,
};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

/// A question the TUI event loop should put in front of the user.
#[derive(Debug)]
pub struct TuiElicitationRequest {
    /// The question text, already shown in the transcript by the caller.
    pub prompt: String,
    /// The multi-field form to render, when the server sent a typed schema.
    ///
    /// `None` is the legacy free-text question. The answer channel then takes
    /// `{"answer": "..."}`; a form takes `{"content": {...}}` or
    /// `{"cancelled": true}`.
    pub form: Option<FormSpec>,
    /// Channel for the user's answer.
    pub response_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// The editable state of a multi-field MCP form in the TUI.
///
/// The server's schema is the source of truth: required fields are marked,
/// defaults are prefilled, and every value is validated before it can be
/// submitted. `focused` is the field the input line edits.
#[derive(Debug, Clone)]
pub struct McpFormState {
    form: FormSpec,
    values: Vec<String>,
    focused: usize,
    error: Option<String>,
}

impl McpFormState {
    /// Start from a parsed schema, prefilling declared defaults.
    #[must_use]
    pub fn new(form: FormSpec) -> Self {
        let values = form.fields.iter().map(default_text).collect();
        Self {
            form,
            values,
            focused: 0,
            error: None,
        }
    }

    /// The schema being filled.
    #[must_use]
    pub fn form(&self) -> &FormSpec {
        &self.form
    }

    /// The index of the focused field.
    #[must_use]
    pub fn focused(&self) -> usize {
        self.focused
    }

    /// The text currently entered for each field.
    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.values
    }

    /// The validation error from the last submit attempt, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Move focus by `delta`, wrapping around.
    pub fn move_focus(&mut self, delta: isize) {
        let count = self.form.fields.len();
        if count == 0 {
            return;
        }
        let next = (self.focused as isize + delta).rem_euclid(count as isize) as usize;
        self.focused = next;
    }

    /// Append a character to the focused field.
    pub fn insert_char(&mut self, c: char) {
        if let Some(value) = self.values.get_mut(self.focused) {
            value.push(c);
        }
    }

    /// Delete the last character of the focused field.
    pub fn backspace(&mut self) {
        if let Some(value) = self.values.get_mut(self.focused) {
            value.pop();
        }
    }

    /// Clear the focused field.
    pub fn clear_field(&mut self) {
        if let Some(value) = self.values.get_mut(self.focused) {
            value.clear();
        }
    }

    /// Validate every value and build the `content` object.
    ///
    /// An empty optional field is omitted (its default, if any, was already
    /// prefilled); an empty required field, a value of the wrong type, an
    /// out-of-range number, or a string that breaks `minLength`/`maxLength`/
    /// `pattern` is an error and nothing is submitted.
    pub fn submit(&mut self) -> Result<Map<String, Value>, String> {
        let mut content = Map::new();
        for (field, text) in self.form.fields.iter().zip(&self.values) {
            let text = text.trim();
            if text.is_empty() {
                if field.required {
                    let message = format!("{} is required", field.label());
                    self.error = Some(message.clone());
                    return Err(message);
                }
                continue;
            }
            match field.value_from_text(text) {
                Ok(value) => {
                    content.insert(field.name.clone(), value);
                }
                Err(reason) => {
                    let message = format!("{}: {reason}", field.label());
                    self.error = Some(message.clone());
                    return Err(message);
                }
            }
        }
        self.error = None;
        Ok(content)
    }

    /// A one-line description of the focused field, for the status line.
    #[must_use]
    pub fn focus_hint(&self) -> String {
        let Some(field) = self.form.fields.get(self.focused) else {
            return "Submit (Enter) or cancel (Esc)".to_string();
        };
        let required = if field.required { " (required)" } else { "" };
        format!(
            "Field {}/{}: {}{}",
            self.focused + 1,
            self.form.fields.len(),
            field.label(),
            required
        )
    }
}

/// The text a field starts with: its default, or empty.
fn default_text(field: &FormField) -> String {
    match &field.default {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
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
        let form: Arc<dyn FormElicitationHandler> = self.clone();
        Arc::new(McpElicitationHandler::with_form(prompt, form))
    }
}

#[async_trait::async_trait]
impl FormElicitationHandler for SessionElicitationHandler {
    async fn request_form(&self, form: &FormSpec) -> FormResult {
        // Clone the sender out of the lock, then release it: the form stays
        // open for as long as the person needs.
        let sender = self.tui.lock().ok().and_then(|slot| slot.as_ref().cloned());

        if let Some(tx) = sender {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let request = TuiElicitationRequest {
                prompt: form.message.clone(),
                form: Some(form.clone()),
                response_tx,
            };
            if tx.send(request).await.is_err() {
                return FormResult::Cancelled;
            }
            return match response_rx.await {
                Ok(value) => {
                    if value
                        .get("cancelled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        return FormResult::Cancelled;
                    }
                    match value.get("content").and_then(Value::as_object) {
                        Some(content) => FormResult::Submitted(content.clone()),
                        None => FormResult::Cancelled,
                    }
                }
                Err(_) => FormResult::Cancelled,
            };
        }

        // No TUI: one JSON object for the whole form. The translation layer
        // validates it against the schema and declines rather than accepting
        // a bad answer.
        if self.interactive {
            let question = ElicitationQuestion {
                question: format!(
                    "{} (answer with one JSON object, e.g. {{\"field\": value}})",
                    form.message
                ),
                header: "MCP form".to_string(),
                options: Vec::new(),
                multi_select: false,
            };
            return match ReplElicitationHandler::default()
                .request(vec![question])
                .await
            {
                ElicitationResult::Responded(response) => {
                    let text = response.other_text.unwrap_or_default();
                    match serde_json::from_str::<Value>(&text) {
                        Ok(Value::Object(content)) => FormResult::Submitted(content),
                        _ => {
                            tracing::warn!("an MCP form answer was not a JSON object; declining");
                            FormResult::Cancelled
                        }
                    }
                }
                _ => FormResult::Cancelled,
            };
        }

        tracing::warn!(
            message = %form.message,
            "an MCP server asked for form input, but this session is non-interactive; answering cancel"
        );
        FormResult::Cancelled
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
                form: None,
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
                .send(serde_json::json!({"content": {"environment": "staging"}}));
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

    /// The whole multi-field schema arrives as one form, and the complete
    /// typed object is returned.
    #[tokio::test]
    async fn a_multi_field_schema_is_one_form_in_the_tui() {
        let handler = SessionElicitationHandler::new(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        handler.set_tui_sender(tx);

        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("the form reached the TUI");
            let form = request.form.expect("a typed schema is a form");
            assert_eq!(form.fields.len(), 2);
            assert_eq!(form.fields[0].name, "environment");
            assert!(form.fields[1].required);
            let _ = request.response_tx.send(serde_json::json!({
                "content": {"environment": "staging", "enabled": true}
            }));
        });

        let answer = handler
            .inbound()
            .handle(
                coderex_mcp::elicitation::ELICITATION_CREATE,
                Some(&serde_json::json!({
                    "message": "Configure the deployment",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {
                            "environment": {"type": "string"},
                            "enabled": {"type": "boolean"}
                        },
                        "required": ["environment", "enabled"]
                    }
                })),
            )
            .await
            .expect("the session answered");

        responder.await.unwrap();
        assert_eq!(answer["action"], "accept");
        assert_eq!(answer["content"]["environment"], "staging");
        assert_eq!(answer["content"]["enabled"], true);
    }

    /// A value the schema rejects must not reach the server as an accept,
    /// even if the form UI submits it.
    #[tokio::test]
    async fn an_invalid_form_value_is_declined() {
        let handler = SessionElicitationHandler::new(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        handler.set_tui_sender(tx);

        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("the form reached the TUI");
            let _ = request.response_tx.send(serde_json::json!({
                "content": {"port": "not-a-number"}
            }));
        });

        let answer = handler
            .inbound()
            .handle(
                coderex_mcp::elicitation::ELICITATION_CREATE,
                Some(&serde_json::json!({
                    "message": "Port",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {"port": {"type": "integer"}},
                        "required": ["port"]
                    }
                })),
            )
            .await
            .expect("the session answered");

        responder.await.unwrap();
        assert_eq!(answer["action"], "decline");
        assert!(answer.get("content").is_none());
    }

    /// A non-interactive session declines a form rather than waiting forever.
    #[tokio::test]
    async fn a_non_interactive_session_cancels_a_form() {
        let handler = SessionElicitationHandler::new(false);
        let answer = handler
            .inbound()
            .handle(
                coderex_mcp::elicitation::ELICITATION_CREATE,
                Some(&serde_json::json!({
                    "message": "Port",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {"port": {"type": "integer"}}
                    }
                })),
            )
            .await
            .expect("the session answered");
        assert_eq!(answer["action"], "cancel");
    }

    fn sample_form() -> FormSpec {
        FormSpec::parse(&serde_json::json!({
            "type": "object",
            "properties": {
                "environment": {"type": "string", "default": "staging"},
                "port": {"type": "integer", "minimum": 1, "maximum": 10},
                "enabled": {"type": "boolean"}
            },
            "required": ["environment", "port"]
        }))
        .expect("a flat schema")
    }

    /// Defaults are prefilled, required fields can be navigated to, and a
    /// valid form submits typed values.
    #[test]
    fn form_state_prefills_defaults_and_submits_typed_values() {
        let mut state = McpFormState::new(sample_form());
        assert_eq!(state.values()[0], "staging");
        assert_eq!(state.focused(), 0);
        assert!(state.focus_hint().contains("required"));

        state.move_focus(1);
        assert_eq!(state.focused(), 1);
        for c in "7".chars() {
            state.insert_char(c);
        }
        state.move_focus(1);
        for c in "true".chars() {
            state.insert_char(c);
        }

        let content = state.submit().expect("a valid form submits");
        assert_eq!(content["environment"], "staging");
        assert_eq!(content["port"], 7);
        assert_eq!(content["enabled"], true);
    }

    /// An empty required field and an out-of-range number are refused, with a
    /// message naming the field, and nothing is submitted.
    #[test]
    fn form_state_refuses_invalid_input() {
        let mut state = McpFormState::new(sample_form());
        let error = state.submit().expect_err("port is required");
        assert!(error.contains("Port") || error.contains("port"), "{error}");
        assert!(state.error().is_some());

        state.move_focus(1);
        for c in "99".chars() {
            state.insert_char(c);
        }
        let error = state.submit().expect_err("port is out of range");
        assert!(error.contains("at most"), "{error}");
    }
}
