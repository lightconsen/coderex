//! MCP prompt discovery and invocation.
//!
//! Servers advertise reusable prompts with `prompts/list`; the client asks for
//! one with `prompts/get`, passing a JSON object of arguments. This module
//! holds the typed shapes for both responses, a registry that tags every
//! prompt with the server that offered it, and the parsing helpers the manager
//! and CLI use.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A single argument declared by an MCP prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    /// Argument name, used verbatim as a key in `prompts/get` arguments.
    pub name: String,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the server requires a value for this argument.
    #[serde(default)]
    pub required: bool,
}

impl McpPromptArgument {
    /// A short display form for listings: `name`, or `name (required)`.
    #[must_use]
    pub fn display(&self) -> String {
        if self.required {
            format!("{} (required)", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// A prompt advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptInfo {
    /// Prompt name, sent verbatim in `prompts/get`.
    pub name: String,
    /// What the prompt is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Arguments the prompt accepts, in the order the server declared them.
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

/// A content block inside a prompt message.
///
/// Image and audio data stay base64-encoded; [`render_text`] deliberately
/// replaces them with a placeholder rather than dumping binary data into the
/// transcript. Unknown block types are preserved as [`Self::Unknown`] instead
/// of failing the whole response.
///
/// [`render_text`]: McpPromptContent::render_text
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpPromptContent {
    /// Plain text.
    Text {
        /// The message text.
        text: String,
    },
    /// An image, base64-encoded.
    Image {
        /// Base64 image data.
        data: String,
        /// MIME type of the image.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Audio, base64-encoded.
    Audio {
        /// Base64 audio data.
        data: String,
        /// MIME type of the audio.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// An embedded resource.
    Resource {
        /// The resource object as the server sent it.
        resource: Value,
    },
    /// A link to a resource.
    ResourceLink {
        /// Resource URI.
        uri: String,
        /// Human-readable name.
        name: String,
        /// Optional description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// MIME type, when supplied.
        #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    /// A content type this client does not render.
    #[serde(other)]
    Unknown,
}

impl McpPromptContent {
    /// Render this block as text for the transcript.
    #[must_use]
    pub fn render_text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Image { mime_type, .. } => format!("[image: {mime_type}]"),
            Self::Audio { mime_type, .. } => format!("[audio: {mime_type}]"),
            Self::Resource { resource } => {
                let uri = resource.get("uri").and_then(Value::as_str);
                let text = resource.get("text").and_then(Value::as_str);
                match (uri, text) {
                    (Some(uri), Some(text)) => format!("[resource: {uri}]\n{text}"),
                    (Some(uri), None) => format!("[resource: {uri}]"),
                    (None, _) => serde_json::to_string_pretty(resource)
                        .unwrap_or_else(|_| "[embedded resource]".to_string()),
                }
            }
            Self::ResourceLink { uri, .. } => format!("[resource: {uri}]"),
            Self::Unknown => "[unsupported content]".to_string(),
        }
    }
}

/// One message in a `prompts/get` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptMessage {
    /// Message role as the server sent it (`user` or `assistant`).
    pub role: String,
    /// The message content.
    pub content: McpPromptContent,
}

/// The result of a `prompts/get` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptResult {
    /// Optional description of the resolved prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Messages the prompt expands to, in order.
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
}

impl McpPromptResult {
    /// Render the result as plain text suitable for a command transcript.
    ///
    /// Each message is labelled with its role. The description, when present,
    /// leads the output so the person can tell which prompt was resolved.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut output = String::new();
        if let Some(ref description) = self.description {
            output.push_str(description);
            output.push('\n');
        }
        if self.messages.is_empty() {
            output.push_str("(the prompt returned no messages)");
            return output;
        }
        for (index, message) in self.messages.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            output.push('[');
            output.push_str(&message.role);
            output.push_str("]\n");
            output.push_str(&message.content.render_text());
        }
        output
    }
}

/// A prompt together with the server that advertised it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerPrompt {
    /// Configured name of the server that offered the prompt.
    pub server: String,
    /// The prompt itself.
    pub prompt: McpPromptInfo,
}

/// Prompts discovered across all connected servers.
#[derive(Debug, Default)]
pub struct McpPromptRegistry {
    prompts: Vec<ServerPrompt>,
}

impl McpPromptRegistry {
    /// Create an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prompts: Vec::new(),
        }
    }

    /// Register one prompt, replacing an entry with the same server and name.
    pub fn register(&mut self, prompt: ServerPrompt) {
        self.prompts.retain(|existing| {
            existing.server != prompt.server || existing.prompt.name != prompt.prompt.name
        });
        self.prompts.push(prompt);
    }

    /// Replace every prompt offered by `server`.
    ///
    /// Used after a fresh `prompts/list`: a prompt the server no longer offers
    /// must disappear rather than linger. The replacement is applied only once
    /// the new list has been parsed, so a failed discovery leaves the previous
    /// list intact.
    pub fn replace_server(&mut self, server: &str, prompts: Vec<McpPromptInfo>) {
        self.prompts.retain(|existing| existing.server != server);
        self.prompts
            .extend(prompts.into_iter().map(|prompt| ServerPrompt {
                server: server.to_string(),
                prompt,
            }));
    }

    /// Find a prompt by server and name.
    #[must_use]
    pub fn get(&self, server: &str, name: &str) -> Option<&ServerPrompt> {
        self.prompts
            .iter()
            .find(|entry| entry.server == server && entry.prompt.name == name)
    }

    /// Every prompt offered by `server`.
    #[must_use]
    pub fn for_server(&self, server: &str) -> Vec<&McpPromptInfo> {
        self.prompts
            .iter()
            .filter(|entry| entry.server == server)
            .map(|entry| &entry.prompt)
            .collect()
    }

    /// All registered prompts.
    #[must_use]
    pub fn list(&self) -> &[ServerPrompt] {
        &self.prompts
    }

    /// Remove every prompt offered by `server`.
    pub fn clear_server(&mut self, server: &str) {
        self.prompts.retain(|entry| entry.server != server);
    }

    /// Number of registered prompts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.prompts.len()
    }

    /// Whether no prompts are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }
}

/// Parse the result of a `prompts/list` request.
///
/// The response must carry a `prompts` array; an entry that cannot be parsed
/// is reported with its index rather than silently dropped.
pub fn parse_prompt_list(response: &Value) -> Result<Vec<McpPromptInfo>, String> {
    let prompts = response
        .get("prompts")
        .and_then(Value::as_array)
        .ok_or_else(|| "prompts/list response has no 'prompts' array".to_string())?;

    prompts
        .iter()
        .enumerate()
        .map(|(index, prompt)| {
            serde_json::from_value(prompt.clone()).map_err(|error| {
                format!("prompts/list entry {index} is not a valid prompt: {error}")
            })
        })
        .collect()
}

/// Parse the result of a `prompts/get` request.
pub fn parse_prompt_result(response: &Value) -> Result<McpPromptResult, String> {
    let messages = response
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "prompts/get response has no 'messages' array".to_string())?;

    let messages = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            serde_json::from_value(message.clone()).map_err(|error| {
                format!("prompts/get message {index} is not a valid prompt message: {error}")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let description = response
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(McpPromptResult {
        description,
        messages,
    })
}

/// Turn command-line tokens into a `prompts/get` arguments object.
///
/// Each token is either `key=value` or a positional value. Positional values
/// fill the prompt's declared arguments in declaration order, skipping names
/// already supplied by a `key=value` token. Values are always strings, which is
/// what the MCP schema requires.
///
/// # Errors
///
/// Returns an error for an empty key, more positional values than declared
/// arguments, or a missing required argument. The server still validates what
/// it receives; this check exists so the person sees the problem locally.
pub fn parse_prompt_arguments(
    tokens: &[String],
    declared: &[McpPromptArgument],
) -> Result<Map<String, Value>, String> {
    let mut arguments = Map::new();
    let mut next_positional = 0_usize;

    for token in tokens {
        if let Some((key, value)) = token.split_once('=') {
            if key.is_empty() {
                return Err(format!(
                    "invalid argument '{token}': the name before '=' is empty"
                ));
            }
            arguments.insert(key.to_string(), Value::String(value.to_string()));
            continue;
        }

        while declared
            .get(next_positional)
            .is_some_and(|argument| arguments.contains_key(&argument.name))
        {
            next_positional += 1;
        }

        let Some(argument) = declared.get(next_positional) else {
            let reason = if declared.is_empty() {
                "this prompt takes no positional arguments"
            } else {
                "too many positional arguments"
            };
            return Err(format!("{reason}: '{token}'"));
        };
        arguments.insert(argument.name.clone(), Value::String(token.clone()));
        next_positional += 1;
    }

    let missing: Vec<&str> = declared
        .iter()
        .filter(|argument| argument.required && !arguments.contains_key(&argument.name))
        .map(|argument| argument.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        ));
    }

    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(name: &str, arguments: Vec<McpPromptArgument>) -> McpPromptInfo {
        McpPromptInfo {
            name: name.to_string(),
            description: Some(format!("the {name} prompt")),
            arguments,
        }
    }

    #[test]
    fn parses_a_prompt_list_with_arguments() {
        let response = serde_json::json!({
            "prompts": [
                {
                    "name": "review",
                    "description": "Review a file",
                    "arguments": [
                        {"name": "path", "description": "File to review", "required": true},
                        {"name": "style"}
                    ]
                },
                {"name": "summarize"}
            ]
        });

        let prompts = parse_prompt_list(&response).unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].name, "review");
        assert_eq!(prompts[0].arguments.len(), 2);
        assert!(prompts[0].arguments[0].required);
        assert_eq!(prompts[0].arguments[0].display(), "path (required)");
        assert_eq!(prompts[0].arguments[1].display(), "style");
        assert_eq!(prompts[1].description, None);
        assert_eq!(prompts[1].arguments, Vec::new());
    }

    #[test]
    fn a_prompt_list_without_the_array_is_an_error() {
        let error = parse_prompt_list(&serde_json::json!({"nope": []})).unwrap_err();
        assert!(error.contains("no 'prompts' array"), "{error}");
    }

    #[test]
    fn a_malformed_prompt_entry_names_its_index() {
        let error =
            parse_prompt_list(&serde_json::json!({"prompts": [{"name": "ok"}, {"nope": 1}]}))
                .unwrap_err();
        assert!(error.contains("entry 1"), "{error}");
    }

    #[test]
    fn parses_text_and_non_text_messages() {
        let response = serde_json::json!({
            "description": "Review result",
            "messages": [
                {"role": "user", "content": {"type": "text", "text": "Please review"}},
                {"role": "assistant", "content": {"type": "image", "data": "AAAA", "mimeType": "image/png"}},
                {"role": "user", "content": {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}}},
                {"role": "assistant", "content": {"type": "mystery"}}
            ]
        });

        let result = parse_prompt_result(&response).unwrap();
        assert_eq!(result.description.as_deref(), Some("Review result"));
        assert_eq!(result.messages.len(), 4);
        assert_eq!(
            result.messages[0].content,
            McpPromptContent::Text {
                text: "Please review".into()
            }
        );
        assert!(matches!(
            result.messages[1].content,
            McpPromptContent::Image { .. }
        ));
        assert_eq!(result.messages[3].content, McpPromptContent::Unknown);

        let rendered = result.render_text();
        assert!(rendered.starts_with("Review result\n[user]\nPlease review"));
        assert!(rendered.contains("[image: image/png]"));
        assert!(
            !rendered.contains("AAAA"),
            "base64 must not reach the transcript"
        );
        assert!(rendered.contains("[resource: file:///a]\nbody"));
        assert!(rendered.contains("[unsupported content]"));
    }

    #[test]
    fn a_prompt_result_without_messages_is_an_error() {
        let error = parse_prompt_result(&serde_json::json!({"description": "x"})).unwrap_err();
        assert!(error.contains("no 'messages' array"), "{error}");
    }

    #[test]
    fn an_empty_result_renders_a_notice() {
        let result = parse_prompt_result(&serde_json::json!({"messages": []})).unwrap();
        assert_eq!(result.render_text(), "(the prompt returned no messages)");
    }

    fn declared() -> Vec<McpPromptArgument> {
        vec![
            McpPromptArgument {
                name: "path".into(),
                description: None,
                required: true,
            },
            McpPromptArgument {
                name: "style".into(),
                description: None,
                required: false,
            },
        ]
    }

    #[test]
    fn positional_tokens_fill_declared_arguments_in_order() {
        let tokens = vec!["src/lib.rs".to_string(), "terse".to_string()];
        let arguments = parse_prompt_arguments(&tokens, &declared()).unwrap();
        assert_eq!(arguments["path"], serde_json::json!("src/lib.rs"));
        assert_eq!(arguments["style"], serde_json::json!("terse"));
    }

    #[test]
    fn key_value_tokens_are_named_and_positional_tokens_skip_them() {
        let tokens = vec!["style=terse".to_string(), "src/lib.rs".to_string()];
        let arguments = parse_prompt_arguments(&tokens, &declared()).unwrap();
        assert_eq!(arguments["style"], serde_json::json!("terse"));
        assert_eq!(arguments["path"], serde_json::json!("src/lib.rs"));
    }

    #[test]
    fn a_missing_required_argument_is_refused() {
        let error = parse_prompt_arguments(&[], &declared()).unwrap_err();
        assert!(
            error.contains("missing required argument(s): path"),
            "{error}"
        );
    }

    #[test]
    fn too_many_positional_arguments_are_refused() {
        let tokens = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let error = parse_prompt_arguments(&tokens, &declared()).unwrap_err();
        assert!(error.contains("too many positional arguments"), "{error}");
    }

    #[test]
    fn a_prompt_without_declared_arguments_refuses_positional_values() {
        let error = parse_prompt_arguments(&["unexpected".to_string()], &[]).unwrap_err();
        assert!(error.contains("takes no positional arguments"), "{error}");
    }

    #[test]
    fn an_empty_key_is_refused() {
        let error = parse_prompt_arguments(&["=value".to_string()], &[]).unwrap_err();
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn registry_tags_prompts_with_their_server_and_replaces_per_server() {
        let mut registry = McpPromptRegistry::new();
        registry.register(ServerPrompt {
            server: "one".into(),
            prompt: prompt("review", Vec::new()),
        });
        registry.register(ServerPrompt {
            server: "two".into(),
            prompt: prompt("review", Vec::new()),
        });
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.for_server("one").len(), 1);
        assert!(registry.get("two", "review").is_some());
        assert!(registry.get("three", "review").is_none());

        registry.replace_server("one", vec![prompt("summarize", Vec::new())]);
        assert!(registry.get("one", "review").is_none());
        assert!(registry.get("one", "summarize").is_some());
        assert!(registry.get("two", "review").is_some());
        assert_eq!(registry.len(), 2);

        registry.clear_server("two");
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }

    #[test]
    fn registering_the_same_prompt_twice_replaces_it() {
        let mut registry = McpPromptRegistry::new();
        registry.register(ServerPrompt {
            server: "one".into(),
            prompt: prompt("review", Vec::new()),
        });
        registry.register(ServerPrompt {
            server: "one".into(),
            prompt: McpPromptInfo {
                name: "review".into(),
                description: Some("updated".into()),
                arguments: Vec::new(),
            },
        });
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.get("one", "review").unwrap().prompt.description,
            Some("updated".into())
        );
    }
}
