//! The action hook kinds — `prompt`, `agent`, and `mcp_tool`.
//!
//! These three kinds need something this crate deliberately does not have: a
//! model client (`prompt` and `agent`) or the MCP server registry (`mcp_tool`).
//! Neither belongs here — `coderex-core`'s API client and `coderex-mcp` are the
//! session's, not the hook system's — so the session that owns them injects one
//! [`HookActionRunner`] with
//! [`HookRegistry::set_action_runner`](crate::HookRegistry::set_action_runner).
//!
//! A hook of these kinds with **no runner installed is a failure**, not a
//! silent skip. Hooks gate work, and a gate that cannot run must deny on a
//! gating event — a kind that quietly did nothing would be an open gate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::engine::{HookInput, parse_hook_output};
use crate::events::{HookError, HookResult};

/// The model an `agent` hook uses when the configuration names none.
///
/// Deliberately the cheap model: an `agent` hook is a verifier that runs on
/// every matching event, and the reference defaults it the same way.
/// (`ModelId::haiku()` in `coderex-core` resolves to this same id.)
pub const AGENT_DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

/// The timeout an `agent` hook uses when the configuration names none (60 s).
///
/// Longer than a `prompt` hook's: the reference describes this kind as an
/// "Agentic verifier hook type", and a verifier that reads the workspace needs
/// more than the 5 s a one-shot question gets.
pub const AGENT_DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// The timeout a `prompt` hook uses when the configuration names none.
pub const PROMPT_DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// The prefix that marks a `${tool_input.<path>}` reference.
const TOOL_INPUT_PREFIX: &str = "${tool_input.";

/// What a non-subprocess hook kind needs from the session that owns it.
///
/// The hooks crate cannot reach a model client or the MCP registry, so the
/// session installs one implementation of this on its
/// [`HookRegistry`](crate::HookRegistry) — the same shape as the rule-exemption
/// resolver `coderex_tools::install_rule_exemptions` installs on the permission
/// engine.
#[async_trait]
pub trait HookActionRunner: Send + Sync {
    /// Answer a `prompt` or `agent` hook: one query, text back.
    ///
    /// The returned text is parsed exactly the way a `command` hook's stdout
    /// is — `{}` or `{"action":"continue"}` continues, `{"action":"stop"}`
    /// stops — so every kind shares one answer format. An empty answer is a
    /// failure, not a continue.
    async fn answer(&self, request: &ActionPrompt) -> Result<String, HookError>;

    /// Call an MCP tool on the named server.
    ///
    /// The returned text is the hook's answer, parsed as above.
    async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<String, HookError>;
}

/// One query for a `prompt` or `agent` hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionPrompt {
    /// The configured prompt, after `$ARGUMENTS` substitution.
    pub prompt: String,
    /// The configured model, or `None` to use the session's default.
    ///
    /// An `agent` hook always has one — the loader fills in
    /// [`AGENT_DEFAULT_MODEL`] when the configuration names none.
    pub model: Option<String>,
    /// Whether this is the `agent` framing rather than a plain `prompt`.
    ///
    /// The runner decides what framing means for its client (the reference
    /// describes it as a verifier); the flag is carried rather than baked in
    /// here because the prompt text is the session's to assemble.
    pub agent: bool,
}

/// The target of an `mcp_tool` hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolHook {
    /// The MCP server to call.
    pub server: String,
    /// The tool on that server.
    pub tool: String,
    /// The tool input, as JSON.
    ///
    /// A **string value** may reference the hook input's `input` field with
    /// `${tool_input.<path>}` — for example `{"file_path":
    /// "${tool_input.file_path}"}`. The reference resolves against the `input`
    /// field parsed as JSON; a path that does not resolve is a **failure**, not
    /// an empty string, so a hook cannot silently call a tool with a missing
    /// argument.
    #[serde(default = "empty_object")]
    pub input: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

impl McpToolHook {
    /// The tool input with every `${tool_input.<path>}` reference resolved
    /// against the hook input.
    ///
    /// # Errors
    ///
    /// Returns the reason a reference could not be resolved.
    pub fn interpolated_input(
        &self,
        hook_input: &HookInput,
    ) -> Result<serde_json::Value, HookError> {
        interpolate_tool_input(&self.input, hook_input)
    }
}

/// Substitute `$ARGUMENTS` (or `${ARGUMENTS}`) in a prompt with the hook input.
///
/// This is how a `prompt` or `agent` hook sees what it is gating: the same JSON
/// a `command` hook receives on stdin. A prompt with no reference is returned
/// unchanged.
#[must_use]
pub fn substitute_arguments(prompt: &str, input_json: &str) -> String {
    prompt
        .replace("$ARGUMENTS", input_json)
        .replace("${ARGUMENTS}", input_json)
}

/// Parse an action hook's answer into a hook result.
///
/// The answer is the same JSON a `command` hook prints on stdout, so all five
/// kinds share one answer format. Empty and unparseable answers follow the
/// command hook's failure rules: silence is not consent.
pub(crate) fn parse_answer(answer: &str) -> Result<HookResult, HookError> {
    if answer.trim().is_empty() {
        return Err(HookError::NoOutput);
    }
    parse_hook_output(answer).map_err(|e| HookError::UnparseableOutput(e.to_string()))
}

/// Resolve every `${tool_input.<path>}` reference in a tool input.
///
/// # Errors
///
/// A reference that cannot be resolved — because the hook input has no `input`
/// field, because that field is not JSON, or because the path does not exist —
/// is an error rather than an empty substitution.
pub fn interpolate_tool_input(
    value: &serde_json::Value,
    hook_input: &HookInput,
) -> Result<serde_json::Value, HookError> {
    if !mentions_tool_input(value) {
        return Ok(value.clone());
    }
    let source = tool_input_source(hook_input)?;
    replace_references(value, &source)
}

/// Whether any string in the value references the hook input.
fn mentions_tool_input(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(TOOL_INPUT_PREFIX),
        serde_json::Value::Array(items) => items.iter().any(mentions_tool_input),
        serde_json::Value::Object(map) => map.values().any(mentions_tool_input),
        _ => false,
    }
}

/// The hook input's `input` field, parsed as JSON.
fn tool_input_source(hook_input: &HookInput) -> Result<serde_json::Value, HookError> {
    let Some(raw) = hook_input.input.as_deref() else {
        return Err(HookError::InterpolationFailed(
            "the hook input has no 'input' field, so ${tool_input.*} cannot be resolved"
                .to_string(),
        ));
    };
    serde_json::from_str(raw).map_err(|e| {
        HookError::InterpolationFailed(format!(
            "the hook input's 'input' field is not JSON, so ${{tool_input.*}} cannot be \
             resolved: {e}"
        ))
    })
}

/// Rebuild the value with every reference in every string resolved.
fn replace_references(
    value: &serde_json::Value,
    source: &serde_json::Value,
) -> Result<serde_json::Value, HookError> {
    match value {
        serde_json::Value::String(text) => {
            Ok(serde_json::Value::String(interpolate_string(text, source)?))
        }
        serde_json::Value::Array(items) => {
            let mut resolved = Vec::with_capacity(items.len());
            for item in items {
                resolved.push(replace_references(item, source)?);
            }
            Ok(serde_json::Value::Array(resolved))
        }
        serde_json::Value::Object(map) => {
            let mut resolved = serde_json::Map::with_capacity(map.len());
            for (key, item) in map {
                resolved.insert(key.clone(), replace_references(item, source)?);
            }
            Ok(serde_json::Value::Object(resolved))
        }
        other => Ok(other.clone()),
    }
}

/// Replace every `${tool_input.<path>}` occurrence in one string.
fn interpolate_string(text: &str, source: &serde_json::Value) -> Result<String, HookError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(TOOL_INPUT_PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start + TOOL_INPUT_PREFIX.len()..];
        let end = after.find('}').ok_or_else(|| {
            HookError::InterpolationFailed(format!(
                "unterminated ${{tool_input. reference in {text:?}"
            ))
        })?;
        let path = &after[..end];
        let value = resolve_path(source, path)?;
        out.push_str(&stringify(&value));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolve a dot-separated path against the hook input's `input` JSON.
///
/// Object keys name fields; a numeric segment indexes an array.
fn resolve_path(source: &serde_json::Value, path: &str) -> Result<serde_json::Value, HookError> {
    let unresolved =
        || HookError::InterpolationFailed(format!("${{tool_input.{path}}} does not resolve"));
    if path.is_empty() {
        return Err(HookError::InterpolationFailed(
            "empty ${tool_input.} reference".to_string(),
        ));
    }

    let mut current = source;
    for segment in path.split('.') {
        current = match current {
            serde_json::Value::Object(map) => map.get(segment).ok_or_else(unresolved)?,
            serde_json::Value::Array(items) => {
                let index: usize = segment.parse().map_err(|_| unresolved())?;
                items.get(index).ok_or_else(unresolved)?
            }
            _ => return Err(unresolved()),
        };
    }
    Ok(current.clone())
}

/// A resolved JSON value as it appears inside a string.
fn stringify(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A runner that records what it was asked and answers from a script.
///
/// Used by this crate's tests — the integration tests define their own, since
/// they are a separate crate.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FakeRunner {
    /// What `answer` replies with.
    answer: String,
    /// What `call_mcp_tool` replies with.
    mcp_answer: String,
    /// The prompts `answer` was called with, in order.
    prompts: std::sync::Mutex<Vec<ActionPrompt>>,
    /// The `(server, tool, input)` triples `call_mcp_tool` was called with.
    mcp_calls: std::sync::Mutex<Vec<(String, String, serde_json::Value)>>,
}

#[cfg(test)]
impl FakeRunner {
    /// A runner whose `answer` and `call_mcp_tool` both return `answer`.
    pub(crate) fn answering(answer: &str) -> std::sync::Arc<Self> {
        Self::new(answer, answer)
    }

    /// A runner whose two entry points answer differently.
    pub(crate) fn new(answer: &str, mcp_answer: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            answer: answer.to_string(),
            mcp_answer: mcp_answer.to_string(),
            prompts: std::sync::Mutex::new(Vec::new()),
            mcp_calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn prompts(&self) -> Vec<ActionPrompt> {
        self.prompts.lock().unwrap().clone()
    }

    pub(crate) fn mcp_calls(&self) -> Vec<(String, String, serde_json::Value)> {
        self.mcp_calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl HookActionRunner for FakeRunner {
    async fn answer(&self, request: &ActionPrompt) -> Result<String, HookError> {
        self.prompts.lock().unwrap().push(request.clone());
        Ok(self.answer.clone())
    }

    async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<String, HookError> {
        self.mcp_calls
            .lock()
            .unwrap()
            .push((server.to_string(), tool.to_string(), input.clone()));
        Ok(self.mcp_answer.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook_input(input: &str) -> HookInput {
        HookInput {
            event: "PreToolUse".to_string(),
            session_id: Some("sess_1".to_string()),
            input: Some(input.to_string()),
            context: None,
        }
    }

    #[test]
    fn substitute_arguments_replaces_both_spellings() {
        assert_eq!(
            substitute_arguments("gating: $ARGUMENTS", r#"{"event":"PreToolUse"}"#),
            r#"gating: {"event":"PreToolUse"}"#
        );
        assert_eq!(
            substitute_arguments("gating: ${ARGUMENTS}", "payload"),
            "gating: payload"
        );
    }

    #[test]
    fn a_prompt_without_a_reference_is_unchanged() {
        assert_eq!(substitute_arguments("is this ok?", "{}"), "is this ok?");
    }

    #[test]
    fn a_string_without_a_reference_is_returned_unchanged() {
        let value = json!({ "file_path": "/tmp/x", "lines": [1, 2] });
        let resolved = interpolate_tool_input(&value, &hook_input("{}")).unwrap();
        assert_eq!(resolved, value);
    }

    #[test]
    fn interpolates_a_tool_input_path_into_a_string_value() {
        let value = json!({ "file_path": "${tool_input.file_path}" });
        let resolved =
            interpolate_tool_input(&value, &hook_input(r#"{"file_path":"/etc/hosts","n":7}"#))
                .unwrap();
        assert_eq!(resolved, json!({ "file_path": "/etc/hosts" }));
    }

    #[test]
    fn interpolates_inside_a_larger_string_and_nested_containers() {
        let value = json!({
            "message": "check ${tool_input.file_path} now",
            "nested": { "list": ["${tool_input.n}", "plain"] }
        });
        let resolved =
            interpolate_tool_input(&value, &hook_input(r#"{"file_path":"/a/b","n":7}"#)).unwrap();
        assert_eq!(
            resolved,
            json!({
                "message": "check /a/b now",
                "nested": { "list": ["7", "plain"] }
            })
        );
    }

    #[test]
    fn interpolates_an_array_index() {
        let value = json!({ "first": "${tool_input.items.0}" });
        let resolved =
            interpolate_tool_input(&value, &hook_input(r#"{"items":["a","b"]}"#)).unwrap();
        assert_eq!(resolved, json!({ "first": "a" }));
    }

    /// A path that does not exist is a failure — not an empty string. A hook
    /// must not silently call a tool with a missing argument.
    #[test]
    fn a_missing_tool_input_path_is_a_failure() {
        let value = json!({ "file_path": "${tool_input.nope}" });
        let err = interpolate_tool_input(&value, &hook_input(r#"{"file_path":"/etc/hosts"}"#))
            .unwrap_err();
        assert!(matches!(err, HookError::InterpolationFailed(_)), "{err:?}");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn an_input_that_is_not_json_is_a_failure() {
        let value = json!({ "x": "${tool_input.a}" });
        let err = interpolate_tool_input(&value, &hook_input("not json")).unwrap_err();
        assert!(matches!(err, HookError::InterpolationFailed(_)), "{err:?}");
    }

    #[test]
    fn a_hook_input_with_no_input_field_is_a_failure() {
        let mut input = hook_input("{}");
        input.input = None;
        let err = interpolate_tool_input(&json!({ "x": "${tool_input.a}" }), &input).unwrap_err();
        assert!(matches!(err, HookError::InterpolationFailed(_)), "{err:?}");
    }

    #[test]
    fn an_unterminated_reference_is_a_failure() {
        let err = interpolate_tool_input(&json!("${tool_input.a"), &hook_input(r#"{"a":1}"#))
            .unwrap_err();
        assert!(matches!(err, HookError::InterpolationFailed(_)), "{err:?}");
    }

    /// Silence is not consent: an empty answer is a failure, and unparseable
    /// text is not a continue.
    #[test]
    fn an_empty_or_unparseable_answer_is_a_failure() {
        assert!(matches!(parse_answer(""), Err(HookError::NoOutput)));
        assert!(matches!(parse_answer("   \n"), Err(HookError::NoOutput)));
        assert!(matches!(
            parse_answer("the tool looks fine to me"),
            Err(HookError::UnparseableOutput(_))
        ));
        assert!(parse_answer(r#"{"action":"continue"}"#).is_ok());
    }

    #[test]
    fn the_default_model_and_timeouts_are_distinct() {
        let agent = AGENT_DEFAULT_TIMEOUT_MS;
        let prompt = PROMPT_DEFAULT_TIMEOUT_MS;
        assert_ne!(agent, prompt);
        assert!(agent > prompt, "an agent hook is the longer-running kind");
        assert!(AGENT_DEFAULT_MODEL.contains("haiku"));
    }
}
