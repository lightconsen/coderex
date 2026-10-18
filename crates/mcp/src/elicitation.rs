//! MCP elicitation — forwarding user prompts/questions from MCP servers.

use crate::channel::{InboundRequestHandler, JsonRpcError, METHOD_NOT_FOUND};
use coderex_core::elicitation::{
    ElicitationHandler, ElicitationQuestion, ElicitationResult as PromptResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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
        let properties = properties_for(&params.requested_schema);

        // A schema with no declared properties has nothing to shape: ask one
        // free-form question and pass a JSON object answer through verbatim.
        if properties.is_empty() {
            return match self.prompt.request(vec![question_for_message(&params)]).await {
                PromptResult::Responded(response) => {
                    let answer = answer_text(&response);
                    McpElicitationResult {
                        action: ElicitationAction::Accept,
                        content: Some(content_from_answer(
                            &params.requested_schema,
                            answer.as_deref(),
                        )),
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
            };
        }

        // Ask for every property, in the order the schema declares them. A
        // property already filled by a JSON object answer (the form-style
        // reply an interactive stdin can provide) is not asked again.
        let mut content = Map::new();
        for (index, property) in properties.iter().enumerate() {
            if content.contains_key(&property.name) {
                continue;
            }

            let question = question_for_property(&params, property, index, properties.len());
            let response = match self.prompt.request(vec![question]).await {
                PromptResult::Responded(response) => response,
                PromptResult::Cancelled => {
                    return McpElicitationResult {
                        action: ElicitationAction::Cancel,
                        content: None,
                    };
                }
                PromptResult::TimedOut => {
                    return McpElicitationResult {
                        action: ElicitationAction::Decline,
                        content: None,
                    };
                }
            };

            match answer_text(&response) {
                Some(answer) => {
                    if let Ok(Value::Object(form)) = serde_json::from_str::<Value>(&answer) {
                        // A JSON object is a complete form: it is what a
                        // headless (interactive stdin) caller provides, so do
                        // not keep asking property by property afterwards.
                        merge_form(&mut content, &form, &properties);
                        break;
                    }
                    content.insert(property.name.clone(), coerce(property, &answer));
                }
                // No value was supplied: a declared default is the answer;
                // otherwise leave it out (the required check below decides).
                None => {
                    if let Some(default) = &property.default {
                        content.insert(property.name.clone(), default.clone());
                    }
                }
            }
        }

        // Defaults also fill properties a JSON form left out.
        for property in &properties {
            if !content.contains_key(&property.name) {
                if let Some(default) = &property.default {
                    content.insert(property.name.clone(), default.clone());
                }
            }
        }

        // An accept must carry every required property. Silence is not an
        // answer: the server would validate the result and reject it, so say
        // decline here instead of sending an incomplete object.
        for property in properties.iter().filter(|property| property.required) {
            if !content.contains_key(&property.name) {
                tracing::warn!(
                    property = %property.name,
                    "the user did not answer a required elicitation property; declining"
                );
                return McpElicitationResult {
                    action: ElicitationAction::Decline,
                    content: None,
                };
            }
        }

        McpElicitationResult {
            action: ElicitationAction::Accept,
            content: Some(Value::Object(content)),
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

/// One flat property of the client-facing elicitation schema.
///
/// The MCP schema is deliberately restricted to flat scalar properties, so
/// this is the whole vocabulary the client has to understand.
struct SchemaProperty {
    /// The key the answer is returned under.
    name: String,
    /// The declared scalar type (`string` when absent).
    kind: PropertyType,
    /// Values an `enum` property allows, as JSON values so non-string enums
    /// still round-trip.
    choices: Vec<Value>,
    /// Whether the server refuses an answer without this property.
    required: bool,
    /// Value to use when the user supplies none.
    default: Option<Value>,
    /// Help text shown with the question.
    description: Option<String>,
}

/// The scalar types an elicitation property may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyType {
    String,
    Boolean,
    Integer,
    Number,
}

/// Collect every property of `requestedSchema`, in schema order.
///
/// `properties` is a JSON object; its iteration order is the order serde_json
/// preserved when the request was parsed. The required list is read as a set:
/// a property named in it must be answered.
fn properties_for(schema: &Value) -> Vec<SchemaProperty> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };

    properties
        .iter()
        .map(|(name, property)| SchemaProperty {
            name: name.clone(),
            kind: property_type(property),
            choices: property
                .get("enum")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            required: required.contains(&name.as_str()),
            default: property.get("default").cloned(),
            description: property
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .collect()
}

/// A property's declared type, tolerating the `["string", "null"]` spelling.
fn property_type(property: &Value) -> PropertyType {
    let declared = match property.get("type") {
        Some(Value::String(name)) => Some(name.as_str()),
        Some(Value::Array(names)) => names.iter().find_map(Value::as_str),
        _ => None,
    };

    match declared {
        Some("boolean") => PropertyType::Boolean,
        Some("integer") => PropertyType::Integer,
        Some("number") => PropertyType::Number,
        _ => PropertyType::String,
    }
}

/// A question for a schema with no declared properties.
fn question_for_message(params: &ElicitationParams) -> ElicitationQuestion {
    ElicitationQuestion {
        question: params.message.clone(),
        header: header_for(&params.message),
        options: Vec::new(),
        multi_select: false,
    }
}

/// Turn one schema property into the engine's question shape.
///
/// An `enum` property becomes numbered choices; every other type is asked as
/// free text and coerced to the declared type when the answer comes back.
fn question_for_property(
    params: &ElicitationParams,
    property: &SchemaProperty,
    index: usize,
    count: usize,
) -> ElicitationQuestion {
    let mut question = params.message.clone();
    if count > 1 {
        question.push_str(&format!(" ({}/{})", index + 1, count));
    }
    question.push_str(&format!(" [{}]", property.name));

    let hint = match property.kind {
        PropertyType::String => None,
        PropertyType::Boolean => Some("true/false"),
        PropertyType::Integer => Some("integer"),
        PropertyType::Number => Some("number"),
    };
    if let Some(hint) = hint {
        question.push_str(&format!(" ({hint})"));
    }
    if let Some(description) = &property.description {
        question.push_str(&format!(" — {description}"));
    }
    if let Some(default) = &property.default {
        question.push_str(&format!(" (default: {default})"));
    }

    let options = property
        .choices
        .iter()
        .map(|value| coderex_core::elicitation::ElicitationOption {
            label: value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string),
            description: property.description.clone().unwrap_or_default(),
        })
        .collect();

    ElicitationQuestion {
        question,
        header: header_for(&property.name),
        options,
        multi_select: false,
    }
}

/// The text of a prompt response: a selected choice wins over free text.
fn answer_text(response: &coderex_core::elicitation::ElicitationResponse) -> Option<String> {
    response
        .selected
        .first()
        .cloned()
        .or_else(|| response.other_text.clone())
        .map(|answer| answer.trim().to_string())
        .filter(|answer| !answer.is_empty())
}

/// A short label for a question, at most 12 characters on a char boundary.
fn header_for(message: &str) -> String {
    const LIMIT: usize = 12;
    if message.chars().count() <= LIMIT {
        return message.to_string();
    }
    let mut header: String = message.chars().take(LIMIT).collect();
    header.push('…');
    header
}

/// Shape a free-text answer for a schema with no declared properties.
///
/// An answer that is itself a JSON object is passed through, which lets a
/// caller fill a schema by hand.
fn content_from_answer(schema: &Value, answer: Option<&str>) -> Value {
    match answer {
        Some(answer) => match serde_json::from_str::<Value>(answer) {
            Ok(Value::Object(object)) => Value::Object(object),
            _ => {
                // There is no property to type the answer as, but a single
                // undeclared property is still a reasonable place to put it.
                let Some(name) = schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .and_then(|properties| properties.keys().next())
                else {
                    return Value::Object(Map::new());
                };
                let mut object = Map::new();
                object.insert(name.clone(), Value::String(answer.to_string()));
                Value::Object(object)
            }
        },
        None => Value::Object(Map::new()),
    }
}

/// Copy the recognised properties of a JSON-object answer into `content`.
///
/// Values are coerced to the declared type, so a form that types `"true"` as
/// text still produces a boolean. Keys the schema does not declare are
/// ignored: the answer has to fit the restricted schema.
fn merge_form(content: &mut Map<String, Value>, form: &Map<String, Value>, properties: &[SchemaProperty]) {
    for property in properties {
        if let Some(value) = form.get(&property.name) {
            // A null is not an answer: leave it out so a default can apply
            // and the required check still sees the property as missing.
            if value.is_null() {
                continue;
            }
            content.insert(
                property.name.clone(),
                coerce_declared(property, value.clone()),
            );
        }
    }
}

/// Coerce a text answer to the type the property declares.
///
/// Elicitation schemas only allow flat scalars, so this is the whole type
/// vocabulary that has to be understood. A value that does not parse is kept
/// as text rather than dropped.
fn coerce(property: &SchemaProperty, answer: &str) -> Value {
    // An enum answer that matches one of the declared values keeps the
    // declared value, including a non-string one.
    if let Some(choice) = property
        .choices
        .iter()
        .find(|choice| choice.as_str() == Some(answer) || choice.to_string() == answer)
    {
        return choice.clone();
    }

    match property.kind {
        PropertyType::Boolean => match answer.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => Value::Bool(true),
            "false" | "no" | "n" | "0" => Value::Bool(false),
            _ => Value::String(answer.to_string()),
        },
        PropertyType::Integer => answer
            .trim()
            .parse::<i64>()
            .map_or_else(|_| Value::String(answer.to_string()), Value::from),
        PropertyType::Number => answer
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| Value::String(answer.to_string()), Value::Number),
        PropertyType::String => Value::String(answer.to_string()),
    }
}

/// Coerce an already-typed JSON value from a form answer.
fn coerce_declared(property: &SchemaProperty, value: Value) -> Value {
    match (&property.kind, &value) {
        (PropertyType::String, Value::String(_)) => value,
        (PropertyType::Boolean, Value::Bool(_)) => value,
        (PropertyType::Integer, Value::Number(number)) if number.as_i64().is_some() => value,
        (PropertyType::Number, Value::Number(_)) => value,
        // A form may send every value as text; reuse the text coercion.
        (_, Value::String(text)) => coerce(property, text),
        _ => value,
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

    /// A prompt that answers each question in turn from a fixed script.
    #[derive(Debug)]
    struct ScriptedPrompt(std::sync::Mutex<std::collections::VecDeque<PromptResult>>);

    impl ScriptedPrompt {
        fn new(results: Vec<PromptResult>) -> Self {
            Self(std::sync::Mutex::new(results.into()))
        }
    }

    #[async_trait::async_trait]
    impl ElicitationHandler for ScriptedPrompt {
        async fn request(&self, _questions: Vec<ElicitationQuestion>) -> PromptResult {
            self.0
                .lock()
                .expect("script")
                .pop_front()
                .expect("the script has an answer for every question")
        }
    }

    fn answered(text: &str) -> PromptResult {
        PromptResult::Responded(coderex_core::elicitation::ElicitationResponse {
            request_id: String::new(),
            selected: Vec::new(),
            other_text: Some(text.into()),
        })
    }

    fn no_answer() -> PromptResult {
        PromptResult::Responded(coderex_core::elicitation::ElicitationResponse {
            request_id: String::new(),
            selected: Vec::new(),
            other_text: None,
        })
    }

    async fn ask_scripted(results: Vec<PromptResult>, schema: Value) -> Value {
        let handler = McpElicitationHandler::new(Arc::new(ScriptedPrompt::new(results)));
        handler
            .handle(
                ELICITATION_CREATE,
                Some(&serde_json::json!({
                    "message": "Configure the deployment",
                    "requestedSchema": schema,
                })),
            )
            .await
            .expect("the handler answered")
    }

    /// Both properties of a two-field schema are collected and typed.
    #[tokio::test]
    async fn a_two_property_schema_answers_every_property() {
        let result = ask_scripted(
            vec![answered("staging"), answered("yes")],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "environment": {"type": "string"},
                    "enabled": {"type": "boolean"}
                },
                "required": ["environment", "enabled"]
            }),
        )
        .await;

        assert_eq!(result["action"], "accept");
        assert_eq!(result["content"]["environment"], "staging");
        assert_eq!(result["content"]["enabled"], true);
    }

    /// An unanswered required property must not turn into an incomplete accept.
    #[tokio::test]
    async fn a_missing_required_property_is_declined() {
        let result = ask_scripted(
            vec![answered("staging"), no_answer()],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "environment": {"type": "string"},
                    "port": {"type": "integer"}
                },
                "required": ["environment", "port"]
            }),
        )
        .await;

        assert_eq!(result["action"], "decline");
        assert!(
            result.get("content").is_none(),
            "an incomplete answer must not carry content: {result}"
        );
    }

    /// A JSON object typed at the first prompt fills the whole schema, the
    /// shape an interactive stdin (headless) can provide.
    #[tokio::test]
    async fn a_json_object_answer_fills_every_property() {
        let result = ask_scripted(
            vec![answered(r#"{"environment":"prod","enabled":true}"#)],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "environment": {"type": "string"},
                    "enabled": {"type": "boolean"}
                },
                "required": ["environment", "enabled"]
            }),
        )
        .await;

        assert_eq!(result["action"], "accept");
        assert_eq!(result["content"]["environment"], "prod");
        assert_eq!(result["content"]["enabled"], true);
    }

    /// A JSON form that omits a required property is still refused.
    #[tokio::test]
    async fn a_json_object_missing_a_required_property_is_declined() {
        let result = ask_scripted(
            vec![answered(r#"{"environment":"prod"}"#), no_answer()],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "environment": {"type": "string"},
                    "enabled": {"type": "boolean"}
                },
                "required": ["environment", "enabled"]
            }),
        )
        .await;

        assert_eq!(result["action"], "decline");
    }

    /// A declared default fills an optional property the form left out.
    #[tokio::test]
    async fn a_default_fills_an_unanswered_optional_property() {
        let result = ask_scripted(
            vec![answered(r#"{"environment":"prod"}"#)],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "environment": {"type": "string"},
                    "retries": {"type": "integer", "default": 3}
                },
                "required": ["environment"]
            }),
        )
        .await;

        assert_eq!(result["action"], "accept");
        assert_eq!(result["content"]["environment"], "prod");
        assert_eq!(result["content"]["retries"], 3);
    }

    /// `enum`, `integer`, and `number` answers keep their declared types.
    #[tokio::test]
    async fn enum_integer_and_number_answers_are_typed() {
        let result = ask_scripted(
            vec![
                PromptResult::Responded(coderex_core::elicitation::ElicitationResponse {
                    request_id: String::new(),
                    selected: vec!["staging".into()],
                    other_text: Some("1".into()),
                }),
                answered("8080"),
                answered("0.5"),
            ],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "environment": {"type": "string", "enum": ["staging", "prod"]},
                    "port": {"type": "integer"},
                    "ratio": {"type": "number"}
                },
                "required": ["environment", "port", "ratio"]
            }),
        )
        .await;

        assert_eq!(result["action"], "accept");
        assert_eq!(result["content"]["environment"], "staging");
        assert_eq!(result["content"]["port"], 8080);
        assert_eq!(result["content"]["ratio"], 0.5);
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
