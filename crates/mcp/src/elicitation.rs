//! MCP elicitation — forwarding user prompts/questions from MCP servers.

use crate::channel::{InboundRequestHandler, JsonRpcError, METHOD_NOT_FOUND};
use coderex_core::elicitation::{
    ElicitationHandler, ElicitationQuestion, ElicitationResult as PromptResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// JSON-RPC's "invalid params" error code.
pub const INVALID_PARAMS: i64 = -32602;

/// The MCP method a server uses to ask the user a question.
pub const ELICITATION_CREATE: &str = "elicitation/create";

/// An elicitation request from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationRequest {
    /// Unique request ID.
    pub id: String,
    /// The server that sent the request.
    pub server: String,
    /// The message/question for the user.
    pub message: String,
    /// Optional suggested responses.
    #[serde(default)]
    pub suggested_responses: Vec<String>,
}

/// A response to an elicitation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    /// The request ID this responds to.
    pub request_id: String,
    /// Whether the user approved/provided a response.
    pub approved: bool,
    /// The user's response text (if any).
    pub text: Option<String>,
}

/// Elicitation manager — pending requests and response routing.
#[derive(Debug, Default)]
pub struct ElicitationManager {
    pending: Vec<ElicitationRequest>,
    responses: Vec<ElicitationResponse>,
}

impl ElicitationManager {
    /// Create a new elicitation manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            responses: Vec::new(),
        }
    }

    /// Submit an elicitation request.
    pub fn request(&mut self, req: ElicitationRequest) {
        self.pending.push(req);
    }

    /// Get pending requests.
    #[must_use]
    pub fn pending(&self) -> &[ElicitationRequest] {
        &self.pending
    }

    /// Respond to a pending request.
    pub fn respond(&mut self, request_id: &str, approved: bool, text: Option<String>) -> bool {
        let idx = self.pending.iter().position(|r| r.id == request_id);
        if let Some(idx) = idx {
            self.pending.remove(idx);
            self.responses.push(ElicitationResponse {
                request_id: request_id.to_string(),
                approved,
                text,
            });
            true
        } else {
            false
        }
    }

    /// Get a response by request ID.
    #[must_use]
    pub fn get_response(&self, request_id: &str) -> Option<&ElicitationResponse> {
        self.responses.iter().find(|r| r.request_id == request_id)
    }

    /// Number of pending requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Clear all pending and responded requests.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.responses.clear();
    }
}

/// Parameters of an MCP `elicitation/create` request.
///
/// `requestedSchema` is the restricted JSON Schema the answer has to fit; it
/// is kept as a `Value` because the client only reads the property names and
/// types it understands, and must not reject a valid schema it does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationParams {
    /// The question to show the user.
    pub message: String,
    /// The shape of the answer the server expects.
    #[serde(default, rename = "requestedSchema")]
    pub requested_schema: Value,
}

/// What the user did with an elicitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    /// The user answered; `content` carries the answer.
    Accept,
    /// The user declined to answer.
    Decline,
    /// The user dismissed the question without deciding.
    Cancel,
}

/// The result of an MCP `elicitation/create` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpElicitationResult {
    /// What the user did.
    pub action: ElicitationAction,
    /// The answer, shaped by `requestedSchema`, when `action` is `accept`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

/// Answers `elicitation/create` by asking the session's user.
///
/// The prompt itself is a [`coderex_core::elicitation::ElicitationHandler`],
/// which is the same interface the engine uses for its own clarification
/// questions: the CLI supplies a handler that routes to the TUI or REPL, and a
/// headless session supplies one that declines. This type only translates
/// between the MCP wire shape and those types.
pub struct McpElicitationHandler {
    prompt: Arc<dyn ElicitationHandler>,
}

impl std::fmt::Debug for McpElicitationHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpElicitationHandler")
    }
}

impl McpElicitationHandler {
    /// Build a handler that asks `prompt` for the answer.
    #[must_use]
    pub fn new(prompt: Arc<dyn ElicitationHandler>) -> Self {
        Self { prompt }
    }

    async fn answer(&self, params: ElicitationParams) -> McpElicitationResult {
        let question = question_for(&params);
        match self.prompt.request(vec![question]).await {
            PromptResult::Responded(response) => {
                let answer = response
                    .selected
                    .first()
                    .cloned()
                    .or_else(|| response.other_text.clone());
                McpElicitationResult {
                    action: ElicitationAction::Accept,
                    content: Some(content_for(&params.requested_schema, answer.as_deref())),
                }
            }
            PromptResult::Cancelled => McpElicitationResult {
                action: ElicitationAction::Cancel,
                content: None,
            },
            PromptResult::TimedOut => McpElicitationResult {
                action: ElicitationAction::Decline,
                content: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl InboundRequestHandler for McpElicitationHandler {
    async fn handle(&self, method: &str, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        if method != ELICITATION_CREATE {
            return Err(JsonRpcError {
                code: METHOD_NOT_FOUND,
                message: format!(
                    "this client has no handler for '{method}'; nothing here can answer it"
                ),
                data: None,
            });
        }

        let params: ElicitationParams =
            serde_json::from_value(params.cloned().unwrap_or(Value::Null)).map_err(|error| {
                JsonRpcError {
                    code: INVALID_PARAMS,
                    message: format!("invalid elicitation/create params: {error}"),
                    data: None,
                }
            })?;

        let result = self.answer(params).await;
        serde_json::to_value(result).map_err(|error| JsonRpcError {
            code: INVALID_PARAMS,
            message: format!("could not serialize the elicitation answer: {error}"),
            data: None,
        })
    }
}

/// Turn the MCP request into the engine's question shape.
///
/// The schema is what tells us whether a free-text answer is enough: a single
/// property with an `enum` becomes a list of choices; anything else is asked
/// as free text and coerced to the property's type when it comes back.
fn question_for(params: &ElicitationParams) -> ElicitationQuestion {
    let mut options = Vec::new();
    let mut property_hint = None;

    if let Some((name, property)) = first_property(&params.requested_schema) {
        property_hint = Some(name.to_string());
        if let Some(values) = property.get("enum").and_then(Value::as_array) {
            for value in values {
                if let Some(label) = value.as_str() {
                    options.push(coderex_core::elicitation::ElicitationOption {
                        label: label.to_string(),
                        description: property
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }
        }
    }

    let mut question = params.message.clone();
    if let Some(hint) = property_hint {
        // The free-text answer is written into this property, so name it.
        question.push_str(&format!(" ({hint})"));
    }

    ElicitationQuestion {
        question,
        header: header_for(&params.message),
        options,
        multi_select: false,
    }
}

/// A short label for the question, at most 12 characters on a char boundary.
fn header_for(message: &str) -> String {
    const LIMIT: usize = 12;
    if message.chars().count() <= LIMIT {
        return message.to_string();
    }
    let mut header: String = message.chars().take(LIMIT).collect();
    header.push('…');
    header
}

/// The first property of the schema, if it has one.
fn first_property(schema: &Value) -> Option<(&str, &Value)> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.iter().next())
        .map(|(name, property)| (name.as_str(), property))
}

/// Shape a free-text answer into the object the server asked for.
///
/// A restricted schema normally has one property; that is the only case a
/// single answer can fill. An answer that is itself a JSON object is passed
/// through, which lets a caller fill a multi-property schema by hand.
fn content_for(schema: &Value, answer: Option<&str>) -> Value {
    let Some(answer) = answer else {
        return Value::Object(serde_json::Map::new());
    };

    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(answer) {
        return Value::Object(object);
    }

    match first_property(schema) {
        Some((name, property)) => {
            let mut object = serde_json::Map::new();
            object.insert(name.to_string(), coerce(property, answer));
            Value::Object(object)
        }
        None => Value::Object(serde_json::Map::new()),
    }
}

/// Coerce a text answer to the type the property declares.
///
/// Elicitation schemas only allow flat scalars, so this is the whole type
/// vocabulary that has to be understood. A value that does not parse is kept
/// as text rather than dropped.
fn coerce(property: &Value, answer: &str) -> Value {
    match property.get("type").and_then(Value::as_str) {
        Some("boolean") => match answer.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => Value::Bool(true),
            "false" | "no" | "n" | "0" => Value::Bool(false),
            _ => Value::String(answer.to_string()),
        },
        Some("integer") => answer
            .trim()
            .parse::<i64>()
            .map_or_else(|_| Value::String(answer.to_string()), Value::from),
        Some("number") => answer
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| Value::String(answer.to_string()), Value::Number),
        _ => Value::String(answer.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> ElicitationRequest {
        ElicitationRequest {
            id: "req1".into(),
            server: "filesystem".into(),
            message: "Allow access to /tmp?".into(),
            suggested_responses: vec!["Yes".into(), "No".into()],
        }
    }

    #[test]
    fn new_is_empty() {
        let mgr = ElicitationManager::new();
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.pending().is_empty());
    }

    #[test]
    fn request_and_respond() {
        let mut mgr = ElicitationManager::new();
        mgr.request(sample_request());
        assert_eq!(mgr.pending_count(), 1);

        let ok = mgr.respond("req1", true, Some("Yes, allow".into()));
        assert!(ok);
        assert_eq!(mgr.pending_count(), 0);

        let resp = mgr.get_response("req1").unwrap();
        assert!(resp.approved);
        assert_eq!(resp.text, Some("Yes, allow".into()));
    }

    #[test]
    fn respond_to_nonexistent_returns_false() {
        let mut mgr = ElicitationManager::new();
        let ok = mgr.respond("nope", true, None);
        assert!(!ok);
    }

    #[test]
    fn deny_response() {
        let mut mgr = ElicitationManager::new();
        mgr.request(sample_request());
        mgr.respond("req1", false, Some("No".into()));
        let resp = mgr.get_response("req1").unwrap();
        assert!(!resp.approved);
    }

    #[test]
    fn clear_removes_all() {
        let mut mgr = ElicitationManager::new();
        mgr.request(sample_request());
        mgr.respond("req1", true, None);
        mgr.clear();
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.get_response("req1").is_none());
    }

    /// A prompt that answers directly, for testing the translation layer.
    #[derive(Debug)]
    struct FixedPrompt(PromptResult);

    #[async_trait::async_trait]
    impl ElicitationHandler for FixedPrompt {
        async fn request(&self, _questions: Vec<ElicitationQuestion>) -> PromptResult {
            self.0.clone()
        }
    }

    async fn ask(result: PromptResult, schema: Value) -> Value {
        let handler = McpElicitationHandler::new(Arc::new(FixedPrompt(result)));
        handler
            .handle(
                ELICITATION_CREATE,
                Some(&serde_json::json!({
                    "message": "Which environment?",
                    "requestedSchema": schema,
                })),
            )
            .await
            .expect("the handler answered")
    }

    #[tokio::test]
    async fn an_accepted_answer_is_shaped_by_the_requested_schema() {
        let result = ask(
            PromptResult::Responded(coderex_core::elicitation::ElicitationResponse {
                request_id: "environment".into(),
                selected: Vec::new(),
                other_text: Some("staging".into()),
            }),
            serde_json::json!({
                "type": "object",
                "properties": {"environment": {"type": "string"}}
            }),
        )
        .await;

        assert_eq!(result["action"], "accept");
        assert_eq!(result["content"]["environment"], "staging");
    }

    #[tokio::test]
    async fn a_numeric_answer_is_coerced_to_the_declared_type() {
        let result = ask(
            PromptResult::Responded(coderex_core::elicitation::ElicitationResponse {
                request_id: "port".into(),
                selected: Vec::new(),
                other_text: Some("8080".into()),
            }),
            serde_json::json!({
                "type": "object",
                "properties": {"port": {"type": "integer"}}
            }),
        )
        .await;

        assert_eq!(result["content"]["port"], 8080);
    }

    #[tokio::test]
    async fn cancelled_becomes_cancel_and_timed_out_becomes_decline() {
        let cancelled = ask(PromptResult::Cancelled, Value::Null).await;
        assert_eq!(cancelled["action"], "cancel");
        assert!(cancelled.get("content").is_none());

        let declined = ask(PromptResult::TimedOut, Value::Null).await;
        assert_eq!(declined["action"], "decline");
    }

    #[tokio::test]
    async fn an_unknown_method_is_refused_without_asking_the_user() {
        let handler = McpElicitationHandler::new(Arc::new(FixedPrompt(PromptResult::Responded(
            coderex_core::elicitation::ElicitationResponse {
                request_id: String::new(),
                selected: Vec::new(),
                other_text: Some("should not be used".into()),
            },
        ))));
        let error = handler
            .handle("sampling/createMessage", None)
            .await
            .expect_err("only elicitation/create is handled");
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }
}
