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

/// The scalar type an elicitation property declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormFieldType {
    /// Free-form text, optionally constrained by `enum`, `minLength`,
    /// `maxLength`, and `pattern`.
    String,
    /// A whole number in `minimum..=maximum`.
    Integer,
    /// A real number in `minimum..=maximum`.
    Number,
    /// A boolean.
    Boolean,
}

/// One flat field of the client-facing elicitation form.
///
/// The MCP schema is deliberately restricted to flat scalar properties. This
/// is that whole vocabulary — plus the constraints the client enforces before
/// it sends an `accept`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    /// The key the answer is returned under.
    pub name: String,
    /// Human-readable label (`title`), when the server supplied one.
    pub title: Option<String>,
    /// Help text shown with the field.
    pub description: Option<String>,
    /// The declared scalar type.
    pub field_type: FormFieldType,
    /// Whether an answer without this field is invalid.
    pub required: bool,
    /// Value to use when the user supplies none.
    pub default: Option<Value>,
    /// Values an `enum` field allows, as JSON values so non-string enums
    /// still round-trip.
    pub choices: Vec<Value>,
    /// Minimum length of a string, in characters.
    pub min_length: Option<usize>,
    /// Maximum length of a string, in characters.
    pub max_length: Option<usize>,
    /// Regular expression a string must match.
    pub pattern: Option<String>,
    /// Inclusive minimum for `integer`/`number`.
    pub minimum: Option<f64>,
    /// Inclusive maximum for `integer`/`number`.
    pub maximum: Option<f64>,
}

/// Why an answer does not fit the requested schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// The field the error belongs to.
    pub field: String,
    /// A human-readable explanation, suitable for a log line.
    pub reason: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

/// A schema this client cannot render or validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormSchemaError {
    /// The schema uses a construct outside MCP's flat elicitation vocabulary
    /// (a nested object/array, `oneOf`, `$ref`, …).
    Unsupported(String),
    /// The schema is malformed in a way that cannot be interpreted.
    Invalid(String),
}

impl std::fmt::Display for FormSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) | Self::Invalid(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for FormSchemaError {}

/// The typed form an `elicitation/create` request asks the user to fill.
///
/// Parsing is deliberately strict: a construct the client cannot faithfully
/// validate (a nested object, an array, `oneOf`) makes the whole schema
/// unsupported rather than silently asking for it as text. The caller declines
/// such a request and says why.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FormSpec {
    /// The server's question.
    pub message: String,
    /// The fields, in schema declaration order.
    pub fields: Vec<FormField>,
}

impl FormSpec {
    /// Parse `requestedSchema` into a form, or say why it cannot be rendered.
    ///
    /// An absent/empty schema yields a form with no fields; callers treat
    /// that as the legacy free-text question.
    pub fn parse(schema: &Value) -> Result<Self, FormSchemaError> {
        if schema.is_null() {
            return Ok(Self::default());
        }
        let Some(object) = schema.as_object() else {
            return Err(FormSchemaError::Invalid(
                "requestedSchema must be a JSON object".to_string(),
            ));
        };
        if object.is_empty() {
            return Ok(Self::default());
        }

        if let Some(reason) = unsupported_marker(object, false) {
            return Err(FormSchemaError::Unsupported(reason));
        }
        if let Some(declared) = object.get("type") {
            match declared {
                Value::String(name) if name == "object" => {}
                Value::Array(names) if names.iter().any(|name| name.as_str() == Some("object")) => {
                }
                other => {
                    return Err(FormSchemaError::Unsupported(format!(
                        "requestedSchema type {other} is not a flat object"
                    )));
                }
            }
        }

        let required: Vec<&str> = object
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let Some(properties) = object.get("properties") else {
            return Ok(Self::default());
        };
        let Some(properties) = properties.as_object() else {
            return Err(FormSchemaError::Invalid(
                "requestedSchema.properties must be an object".to_string(),
            ));
        };

        let mut fields = Vec::with_capacity(properties.len());
        for (name, property) in properties {
            let property = property.as_object().ok_or_else(|| {
                FormSchemaError::Invalid(format!("property '{name}' is not an object"))
            })?;
            if let Some(reason) = unsupported_marker(property, true) {
                return Err(FormSchemaError::Unsupported(format!(
                    "property '{name}': {reason}"
                )));
            }
            let field_type = parse_field_type(name, property)?;
            fields.push(FormField {
                name: name.clone(),
                title: property
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                description: property
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                field_type,
                required: required.contains(&name.as_str()),
                default: property.get("default").cloned(),
                choices: property
                    .get("enum")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                min_length: unsigned_property(property, "minLength"),
                max_length: unsigned_property(property, "maxLength"),
                pattern: property
                    .get("pattern")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                minimum: number_property(property, "minimum"),
                maximum: number_property(property, "maximum"),
            });
        }

        Ok(Self {
            message: String::new(),
            fields,
        })
    }

    /// Set the question the form belongs to.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Fill every absent field that declares a `default`.
    pub fn apply_defaults(&self, values: &mut Map<String, Value>) {
        for field in &self.fields {
            if !values.contains_key(&field.name) {
                if let Some(default) = &field.default {
                    values.insert(field.name.clone(), default.clone());
                }
            }
        }
    }

    /// Validate a complete answer object against the schema.
    ///
    /// Every declared field is checked: required presence, JSON type, `enum`
    /// membership, string length/pattern, and numeric range. Unknown keys are
    /// ignored, but a key the schema declares is never allowed to carry a
    /// value of the wrong shape.
    pub fn validate_values(&self, values: &Map<String, Value>) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        for field in &self.fields {
            match values.get(&field.name) {
                None => {
                    if field.required {
                        errors.push(ValidationError {
                            field: field.name.clone(),
                            reason: "is required".to_string(),
                        });
                    }
                }
                Some(value) => {
                    if let Err(reason) = field.validate_value(value) {
                        errors.push(ValidationError {
                            field: field.name.clone(),
                            reason,
                        });
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// The unsupported constructs that make a schema unrenderable.
///
/// At the top level `properties` is the form itself; inside a property it is a
/// nested object, which is outside MCP's flat vocabulary.
fn unsupported_marker(object: &Map<String, Value>, nested: bool) -> Option<String> {
    const MARKERS: &[&str] = &[
        "oneOf", "anyOf", "allOf", "not", "$ref", "items", "if", "then", "else",
    ];
    if let Some(marker) = MARKERS.iter().find(|marker| object.contains_key(**marker)) {
        return Some(format!(
            "'{marker}' is outside MCP's flat elicitation schema"
        ));
    }
    if nested && object.contains_key("properties") {
        return Some("a nested object is outside MCP's flat elicitation schema".to_string());
    }
    None
}

/// A field's declared scalar type.
fn parse_field_type(
    name: &str,
    property: &Map<String, Value>,
) -> Result<FormFieldType, FormSchemaError> {
    let scalar = |value: &str| match value {
        "string" => Some(FormFieldType::String),
        "integer" => Some(FormFieldType::Integer),
        "number" => Some(FormFieldType::Number),
        "boolean" => Some(FormFieldType::Boolean),
        _ => None,
    };

    match property.get("type") {
        None => {
            // An `enum` without a type is the string spelling used by several
            // servers; with no enum there is nothing to shape it, so it is
            // treated as free text.
            Ok(FormFieldType::String)
        }
        Some(Value::String(declared)) => scalar(declared).ok_or_else(|| {
            FormSchemaError::Unsupported(format!(
                "property '{name}' has type '{declared}', which is not a flat scalar"
            ))
        }),
        Some(Value::Array(declared)) => {
            let mut chosen = None;
            for value in declared {
                let Some(declared_type) = value.as_str() else {
                    return Err(FormSchemaError::Invalid(format!(
                        "property '{name}' has a non-string type"
                    )));
                };
                if declared_type == "null" {
                    continue;
                }
                let Some(field_type) = scalar(declared_type) else {
                    return Err(FormSchemaError::Unsupported(format!(
                        "property '{name}' has type '{declared_type}', which is not a flat scalar"
                    )));
                };
                if chosen.is_some() {
                    return Err(FormSchemaError::Unsupported(format!(
                        "property '{name}' declares more than one type"
                    )));
                }
                chosen = Some(field_type);
            }
            Ok(chosen.unwrap_or(FormFieldType::String))
        }
        Some(other) => Err(FormSchemaError::Invalid(format!(
            "property '{name}' has an invalid type: {other}"
        ))),
    }
}

/// An unsigned schema keyword (`minLength`, `maxLength`).
fn unsigned_property(property: &Map<String, Value>, key: &str) -> Option<usize> {
    property
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

/// A numeric schema keyword (`minimum`, `maximum`).
fn number_property(property: &Map<String, Value>, key: &str) -> Option<f64> {
    property.get(key).and_then(Value::as_f64)
}

impl FormField {
    /// The label to show for this field.
    #[must_use]
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }

    /// Coerce a text answer to this field's declared type.
    ///
    /// A value that does not parse is kept as text; validation then rejects it
    /// instead of the client pretending it was a number or a boolean.
    #[must_use]
    pub fn coerce_text(&self, answer: &str) -> Value {
        let answer = answer.trim();
        // An enum answer that matches one of the declared values keeps the
        // declared value, including a non-string one.
        if let Some(choice) = self
            .choices
            .iter()
            .find(|choice| choice.as_str() == Some(answer) || choice.to_string() == answer)
        {
            return choice.clone();
        }

        match self.field_type {
            FormFieldType::Boolean => match answer.to_ascii_lowercase().as_str() {
                "true" | "yes" | "y" | "1" => Value::Bool(true),
                "false" | "no" | "n" | "0" => Value::Bool(false),
                _ => Value::String(answer.to_string()),
            },
            FormFieldType::Integer => answer
                .parse::<i64>()
                .map_or_else(|_| Value::String(answer.to_string()), Value::from),
            FormFieldType::Number => answer
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map_or_else(|| Value::String(answer.to_string()), Value::Number),
            FormFieldType::String => Value::String(answer.to_string()),
        }
    }

    /// Coerce text and validate it, for a UI that rejects bad input inline.
    pub fn value_from_text(&self, answer: &str) -> Result<Value, String> {
        let value = self.coerce_text(answer);
        self.validate_value(&value)?;
        Ok(value)
    }

    /// Validate one value against this field's constraints.
    pub fn validate_value(&self, value: &Value) -> Result<(), String> {
        if !self.choices.is_empty() && !self.choices.iter().any(|allowed| allowed == value) {
            return Err(format!(
                "must be one of {}",
                self.choices
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        match (&self.field_type, value) {
            (FormFieldType::String, Value::String(text)) => {
                let length = text.chars().count();
                if let Some(minimum) = self.min_length {
                    if length < minimum {
                        return Err(format!("must be at least {minimum} characters"));
                    }
                }
                if let Some(maximum) = self.max_length {
                    if length > maximum {
                        return Err(format!("must be at most {maximum} characters"));
                    }
                }
                if let Some(pattern) = &self.pattern {
                    let regex = regex::Regex::new(pattern)
                        .map_err(|error| format!("has an invalid pattern: {error}"))?;
                    if !regex.is_match(text) {
                        return Err(format!("must match the pattern {pattern}"));
                    }
                }
                Ok(())
            }
            (FormFieldType::Integer, Value::Number(number)) => {
                if number.as_i64().is_none() && number.as_u64().is_none() {
                    return Err("must be an integer".to_string());
                }
                self.check_range(number.as_f64().unwrap_or_default())
            }
            (FormFieldType::Number, Value::Number(number)) => {
                self.check_range(number.as_f64().unwrap_or_default())
            }
            (FormFieldType::Boolean, Value::Bool(_)) => Ok(()),
            (expected, actual) => Err(format!(
                "must be {} (got {})",
                match expected {
                    FormFieldType::String => "a string",
                    FormFieldType::Integer => "an integer",
                    FormFieldType::Number => "a number",
                    FormFieldType::Boolean => "a boolean",
                },
                type_name(actual)
            )),
        }
    }

    fn check_range(&self, value: f64) -> Result<(), String> {
        if let Some(minimum) = self.minimum {
            if value < minimum {
                return Err(format!("must be at least {minimum}"));
            }
        }
        if let Some(maximum) = self.maximum {
            if value > maximum {
                return Err(format!("must be at most {maximum}"));
            }
        }
        Ok(())
    }
}

/// The JSON type name of a value, for validation messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// How a form-capable prompt answers.
#[derive(Debug, Clone)]
pub enum FormResult {
    /// The user filled the form.
    Submitted(Map<String, Value>),
    /// The user dismissed the form.
    Cancelled,
    /// Nobody answered in time.
    TimedOut,
}

/// A prompt that can present every field of a schema at once.
///
/// A prompt that only implements [`ElicitationHandler`] is still supported:
/// the handler then asks one field at a time and still validates the assembled
/// answer. This trait is what lets the TUI show a real multi-field form.
#[async_trait::async_trait]
pub trait FormElicitationHandler: Send + Sync + std::fmt::Debug {
    /// Present `form` and return the values the user submitted.
    async fn request_form(&self, form: &FormSpec) -> FormResult;
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
    form_prompt: Option<Arc<dyn FormElicitationHandler>>,
}

impl std::fmt::Debug for McpElicitationHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpElicitationHandler")
            .field("form_capable", &self.form_prompt.is_some())
            .finish()
    }
}

impl McpElicitationHandler {
    /// Build a handler that asks `prompt` for the answer.
    #[must_use]
    pub fn new(prompt: Arc<dyn ElicitationHandler>) -> Self {
        Self {
            prompt,
            form_prompt: None,
        }
    }

    /// Build a handler whose prompt can render the whole form at once.
    #[must_use]
    pub fn with_form(
        prompt: Arc<dyn ElicitationHandler>,
        form_prompt: Arc<dyn FormElicitationHandler>,
    ) -> Self {
        Self {
            prompt,
            form_prompt: Some(form_prompt),
        }
    }

    /// Answer a parsed request, declining anything the client cannot validate.
    async fn answer(&self, params: ElicitationParams) -> McpElicitationResult {
        let spec = match FormSpec::parse(&params.requested_schema) {
            Ok(spec) => spec.with_message(params.message.clone()),
            Err(error) => {
                tracing::warn!(
                    reason = %error,
                    "declining an elicitation whose requestedSchema this client cannot handle"
                );
                return McpElicitationResult {
                    action: ElicitationAction::Decline,
                    content: None,
                };
            }
        };

        // A schema with no declared properties has nothing to shape: ask one
        // free-form question and pass a JSON object answer through verbatim.
        if spec.fields.is_empty() {
            return match self
                .prompt
                .request(vec![question_for_message(&params)])
                .await
            {
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

        if let Some(form_prompt) = &self.form_prompt {
            let mut content = match form_prompt.request_form(&spec).await {
                FormResult::Submitted(content) => content,
                FormResult::Cancelled => {
                    return McpElicitationResult {
                        action: ElicitationAction::Cancel,
                        content: None,
                    };
                }
                FormResult::TimedOut => {
                    return McpElicitationResult {
                        action: ElicitationAction::Decline,
                        content: None,
                    };
                }
            };
            return finish(spec, &mut content);
        }

        // Ask for every property, in the order the schema declares them. A
        // property already filled by a JSON object answer (the headless
        // form-style reply) is not asked again.
        let mut content = Map::new();
        for (index, field) in spec.fields.iter().enumerate() {
            let question = question_for_field(&spec, field, index);
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
                        merge_form(&mut content, &form, &spec.fields);
                        break;
                    }
                    content.insert(field.name.clone(), field.coerce_text(&answer));
                }
                // No value was supplied: a default may fill it; otherwise
                // validation decides whether it was required.
                None => {}
            }
        }

        finish(spec, &mut content)
    }
}

/// Apply defaults, validate, and turn the result into an `accept` or a
/// `decline`.
///
/// Validation is the last word: a wrong type, a missing required field, a
/// range or pattern violation never becomes an `accept`.
fn finish(spec: FormSpec, content: &mut Map<String, Value>) -> McpElicitationResult {
    spec.apply_defaults(content);
    match spec.validate_values(content) {
        Ok(()) => McpElicitationResult {
            action: ElicitationAction::Accept,
            content: Some(Value::Object(std::mem::take(content))),
        },
        Err(errors) => {
            for error in &errors {
                tracing::warn!(
                    field = %error.field,
                    reason = %error.reason,
                    "an elicitation answer did not fit the requested schema; declining"
                );
            }
            McpElicitationResult {
                action: ElicitationAction::Decline,
                content: None,
            }
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

/// A question for a schema with no declared properties.
fn question_for_message(params: &ElicitationParams) -> ElicitationQuestion {
    ElicitationQuestion {
        question: params.message.clone(),
        header: header_for(&params.message),
        options: Vec::new(),
        multi_select: false,
    }
}

/// Turn one schema field into the engine's single-question shape.
///
/// A form-capable prompt does not use this; it is the fallback for prompts
/// that only implement [`ElicitationHandler`].
fn question_for_field(spec: &FormSpec, field: &FormField, index: usize) -> ElicitationQuestion {
    let count = spec.fields.len();
    let mut question = spec.message.clone();
    if count > 1 {
        question.push_str(&format!(" ({}/{})", index + 1, count));
    }
    question.push_str(&format!(" [{}]", field.label()));
    if field.required {
        question.push_str(" (required)");
    }

    match field.field_type {
        FormFieldType::String => {
            if let Some(minimum) = field.min_length {
                question.push_str(&format!(" (min {minimum} chars)"));
            }
            if let Some(maximum) = field.max_length {
                question.push_str(&format!(" (max {maximum} chars)"));
            }
            if let Some(pattern) = &field.pattern {
                question.push_str(&format!(" (pattern: {pattern})"));
            }
        }
        FormFieldType::Boolean => question.push_str(" (true/false)"),
        FormFieldType::Integer => question.push_str(" (integer)"),
        FormFieldType::Number => question.push_str(" (number)"),
    }
    if field.minimum.is_some() || field.maximum.is_some() {
        question.push_str(&format!(
            " (range: {}..={})",
            field
                .minimum
                .map_or_else(|| "-inf".to_string(), |value| value.to_string()),
            field
                .maximum
                .map_or_else(|| "+inf".to_string(), |value| value.to_string()),
        ));
    }
    if let Some(description) = &field.description {
        question.push_str(&format!(" — {description}"));
    }
    if let Some(default) = &field.default {
        question.push_str(&format!(" (default: {default})"));
    }

    let options = field
        .choices
        .iter()
        .map(|value| coderex_core::elicitation::ElicitationOption {
            label: value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string),
            description: field.description.clone().unwrap_or_default(),
        })
        .collect();

    ElicitationQuestion {
        question,
        header: header_for(field.label()),
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
                // There is no property to type the answer as.
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

/// Copy the recognised fields of a JSON-object answer into `content`.
///
/// Values are coerced to the declared type, so a form that types `"true"` as
/// text still produces a boolean. Keys the schema does not declare are
/// ignored: the answer has to fit the restricted schema.
fn merge_form(content: &mut Map<String, Value>, form: &Map<String, Value>, fields: &[FormField]) {
    for field in fields {
        if let Some(value) = form.get(&field.name) {
            // A null is not an answer: leave it out so a default can apply
            // and the required check still sees the property as missing.
            if value.is_null() {
                continue;
            }
            let value = match value {
                Value::String(text) => field.coerce_text(text),
                other => other.clone(),
            };
            content.insert(field.name.clone(), value);
        }
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

    #[test]
    fn parses_every_declared_scalar_constraint() {
        let spec = FormSpec::parse(&serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "title": "Name",
                    "description": "The deployment name",
                    "minLength": 2,
                    "maxLength": 10,
                    "pattern": "^[a-z]+$",
                    "default": "staging"
                },
                "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                "ratio": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                "enabled": {"type": "boolean"},
                "region": {"type": "string", "enum": ["us", "eu"]}
            },
            "required": ["name", "port"]
        }))
        .expect("a flat schema parses");

        assert_eq!(spec.fields.len(), 5);
        assert_eq!(spec.fields[0].name, "name");
        assert_eq!(spec.fields[0].title.as_deref(), Some("Name"));
        assert_eq!(spec.fields[0].min_length, Some(2));
        assert_eq!(spec.fields[0].max_length, Some(10));
        assert_eq!(spec.fields[0].pattern.as_deref(), Some("^[a-z]+$"));
        assert_eq!(
            spec.fields[0].default,
            Some(Value::String("staging".into()))
        );
        assert!(spec.fields[0].required);
        assert_eq!(spec.fields[1].field_type, FormFieldType::Integer);
        assert_eq!(spec.fields[1].minimum, Some(1.0));
        assert_eq!(spec.fields[1].maximum, Some(65535.0));
        assert_eq!(spec.fields[2].field_type, FormFieldType::Number);
        assert_eq!(spec.fields[3].field_type, FormFieldType::Boolean);
        assert_eq!(spec.fields[4].choices.len(), 2);
        assert!(spec.fields[1].required);
        assert!(!spec.fields[2].required);
    }

    fn typed_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "pattern": "^[a-z]+$", "minLength": 2},
                "port": {"type": "integer", "minimum": 1, "maximum": 10},
                "enabled": {"type": "boolean"}
            },
            "required": ["name", "port", "enabled"]
        })
    }

    /// A value of the wrong type never becomes an `accept`.
    #[tokio::test]
    async fn a_wrong_type_is_declined() {
        let result = ask_scripted(
            vec![
                answered("staging"),
                answered("not-a-number"),
                answered("true"),
            ],
            typed_schema(),
        )
        .await;
        assert_eq!(result["action"], "decline");
        assert!(result.get("content").is_none());
    }

    /// A number outside `minimum..=maximum` never becomes an `accept`.
    #[tokio::test]
    async fn an_out_of_range_number_is_declined() {
        let result = ask_scripted(
            vec![answered("staging"), answered("50"), answered("true")],
            typed_schema(),
        )
        .await;
        assert_eq!(result["action"], "decline");
        assert!(result.get("content").is_none());
    }

    /// A string that breaks the declared `pattern` never becomes an `accept`.
    #[tokio::test]
    async fn a_pattern_violation_is_declined() {
        let result = ask_scripted(
            vec![answered("STAGING"), answered("5"), answered("true")],
            typed_schema(),
        )
        .await;
        assert_eq!(result["action"], "decline");
        assert!(result.get("content").is_none());
    }

    /// `minLength` is enforced even though the value has the right type.
    #[tokio::test]
    async fn a_too_short_string_is_declined() {
        let result = ask_scripted(
            vec![answered("x"), answered("5"), answered("true")],
            typed_schema(),
        )
        .await;
        assert_eq!(result["action"], "decline");
        assert!(result.get("content").is_none());
    }

    /// An enum value outside the declared choices never becomes an `accept`.
    #[tokio::test]
    async fn an_out_of_enum_value_is_declined() {
        let result = ask_scripted(
            vec![answered("asia")],
            serde_json::json!({
                "type": "object",
                "properties": {"region": {"type": "string", "enum": ["us", "eu"]}},
                "required": ["region"]
            }),
        )
        .await;
        assert_eq!(result["action"], "decline");
    }

    /// A headless JSON-object answer with a bad value is declined too.
    #[tokio::test]
    async fn a_json_object_with_a_bad_value_is_declined() {
        let result = ask_scripted(
            vec![answered(r#"{"name":"STAGING","port":5,"enabled":true}"#)],
            typed_schema(),
        )
        .await;
        assert_eq!(result["action"], "decline");
    }

    /// A nested object is outside MCP's restricted elicitation schema; it is
    /// declined without asking the user to type it as text.
    #[tokio::test]
    async fn a_nested_object_schema_is_declined_without_asking() {
        let result = ask_scripted(
            Vec::new(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "config": {
                        "type": "object",
                        "properties": {"nested": {"type": "string"}}
                    }
                }
            }),
        )
        .await;
        assert_eq!(result["action"], "decline");
    }

    /// An array and `oneOf` are likewise unsupported, not asked as text.
    #[tokio::test]
    async fn array_and_one_of_schemas_are_declined_without_asking() {
        let array = ask_scripted(
            Vec::new(),
            serde_json::json!({
                "type": "object",
                "properties": {"tags": {"type": "array", "items": {"type": "string"}}}
            }),
        )
        .await;
        assert_eq!(array["action"], "decline");

        let one_of = ask_scripted(
            Vec::new(),
            serde_json::json!({
                "type": "object",
                "properties": {"value": {"oneOf": [{"type": "string"}, {"type": "integer"}]}}
            }),
        )
        .await;
        assert_eq!(one_of["action"], "decline");
    }
}
