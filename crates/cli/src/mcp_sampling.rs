//! MCP sampling: a server asking the session's model to generate text.
//!
//! `sampling/createMessage` spends the person's tokens on a server's behalf,
//! so it is denied unless `CODEREX_MCP_SAMPLING=allow` is set explicitly.
//! When it is, this module forwards the server's conversation to the session's
//! own API client, clamping the requested budget to a ceiling so a server
//! cannot ask for an unbounded completion. Every failure is returned to the
//! server as a JSON-RPC error; nothing here panics.

use coderex_core::api_client::ApiClient;
use coderex_core::types::ModelId;
use coderex_mcp::channel::JsonRpcError;
use coderex_mcp::elicitation::{
    INTERNAL_ERROR, INVALID_PARAMS, ModelPreferences, SAMPLING_OPT_IN_ENV, SAMPLING_OPT_IN_VALUE,
    SamplingContent, SamplingHandler, SamplingRequest, SamplingResult, SamplingRole,
};
use std::sync::Arc;

/// The ceiling used when `CODEREX_MCP_SAMPLING_MAX_TOKENS` is unset.
///
/// A server may ask for more, but this is as many output tokens as it can
/// spend in one request.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// The environment variable that raises or lowers the token ceiling.
pub const MAX_TOKENS_ENV: &str = "CODEREX_MCP_SAMPLING_MAX_TOKENS";

/// Whether the person opted into MCP sampling.
///
/// Only [`SAMPLING_OPT_IN_VALUE`] enables it. An unset or unrecognized value
/// keeps the default, which is deny.
#[must_use]
pub fn enabled_from_env() -> bool {
    match std::env::var(SAMPLING_OPT_IN_ENV) {
        Ok(value) if value.trim().eq_ignore_ascii_case(SAMPLING_OPT_IN_VALUE) => true,
        Ok(value) if !value.trim().is_empty() => {
            tracing::warn!(
                value = %value,
                "unrecognized {SAMPLING_OPT_IN_ENV}; MCP sampling stays disabled (expected \
                 {SAMPLING_OPT_IN_VALUE})"
            );
            false
        }
        _ => false,
    }
}

/// The effective per-request token ceiling.
///
/// A positive integer in [`MAX_TOKENS_ENV`] wins; anything else falls back to
/// [`DEFAULT_MAX_TOKENS`] rather than failing the request.
#[must_use]
pub fn max_tokens_ceiling_from_env() -> u32 {
    std::env::var(MAX_TOKENS_ENV).map_or(DEFAULT_MAX_TOKENS, |value| {
        match value.trim().parse::<u32>() {
            Ok(tokens) if tokens > 0 => tokens,
            _ => {
                tracing::warn!(
                    value = %value,
                    "ignoring {MAX_TOKENS_ENV}: expected a positive integer; using \
                     {DEFAULT_MAX_TOKENS}"
                );
                DEFAULT_MAX_TOKENS
            }
        }
    })
}

/// The sampling handler offered to a session's MCP manager.
///
/// It owns the same API client the session runs on and the model the session
/// resolved. `max_tokens_ceiling` is captured when the handler is built so the
/// environment is read once per session.
pub struct CliSamplingHandler {
    client: Box<dyn ApiClient>,
    session_model: ModelId,
    max_tokens_ceiling: u32,
}

impl std::fmt::Debug for CliSamplingHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliSamplingHandler")
            .field("session_model", &self.session_model)
            .field("max_tokens_ceiling", &self.max_tokens_ceiling)
            // The client is a trait object without `Debug`.
            .finish_non_exhaustive()
    }
}

impl CliSamplingHandler {
    /// Build a handler with the environment's token ceiling.
    #[must_use]
    pub fn new(client: Box<dyn ApiClient>, session_model: ModelId) -> Self {
        Self::with_ceiling(client, session_model, max_tokens_ceiling_from_env())
    }

    /// Build a handler with an explicit ceiling.
    #[must_use]
    pub fn with_ceiling(
        client: Box<dyn ApiClient>,
        session_model: ModelId,
        max_tokens_ceiling: u32,
    ) -> Self {
        Self {
            client,
            session_model,
            max_tokens_ceiling,
        }
    }

    /// The ceiling this handler applies.
    #[must_use]
    pub const fn max_tokens_ceiling(&self) -> u32 {
        self.max_tokens_ceiling
    }

    /// The model to answer with: the first known hint, else the session model.
    fn requested_model(&self, preferences: Option<&ModelPreferences>) -> ModelId {
        preferences
            .into_iter()
            .flat_map(|preferences| preferences.hints.iter())
            .filter_map(|hint| hint.name.as_deref())
            .find_map(known_model)
            .unwrap_or_else(|| self.session_model.clone())
    }
}

/// Map a server's model hint to a model CodeRex knows.
///
/// An unknown name is not forwarded: the provider would reject it, and a
/// server should not be able to name an arbitrary model.
fn known_model(name: &str) -> Option<ModelId> {
    match name.trim().to_ascii_lowercase().as_str() {
        "opus" | "claude-opus-4-6" => Some(ModelId::opus()),
        "sonnet" | "claude-sonnet-4-6" => Some(ModelId::sonnet()),
        "haiku" | "claude-haiku-4-5-20251001" => Some(ModelId::haiku()),
        _ => None,
    }
}

#[async_trait::async_trait]
impl SamplingHandler for CliSamplingHandler {
    async fn sample(&self, request: &SamplingRequest) -> Result<SamplingResult, JsonRpcError> {
        let model = self.requested_model(request.model_preferences.as_ref());
        let max_tokens = request.max_tokens.min(self.max_tokens_ceiling);
        let system = request.system_prompt.as_deref().unwrap_or_default();

        let mut messages = Vec::with_capacity(request.messages.len());
        for (index, message) in request.messages.iter().enumerate() {
            let Some(text) = message.content.as_text() else {
                return Err(JsonRpcError {
                    code: INVALID_PARAMS,
                    message: format!(
                        "sampling message {index} has content type '{}'; this client forwards \
                         text only",
                        message.content.kind
                    ),
                    data: None,
                });
            };
            messages.push(serde_json::json!({
                "role": message.role.as_str(),
                "content": text,
            }));
        }

        if messages.is_empty() {
            return Err(JsonRpcError {
                code: INVALID_PARAMS,
                message: "the sampling request contained no messages to send".to_string(),
                data: None,
            });
        }

        let response = self
            .client
            .create_message(system, &messages, &model, max_tokens, &[])
            .await
            .map_err(|error| JsonRpcError {
                code: INTERNAL_ERROR,
                message: format!("the session model could not answer a sampling request: {error}"),
                data: None,
            })?;

        Ok(SamplingResult {
            role: SamplingRole::Assistant,
            content: SamplingContent::text(response.content),
            model: model.0,
        })
    }
}

/// Build the handler to install, or `None` when sampling is not enabled.
///
/// The caller installs it only when this returns `Some`, which keeps the
/// default a refusal.
#[must_use]
pub fn handler_from_env(
    client: Box<dyn ApiClient>,
    session_model: ModelId,
) -> Option<Arc<dyn SamplingHandler>> {
    enabled_from_env().then(|| {
        Arc::new(CliSamplingHandler::new(client, session_model)) as Arc<dyn SamplingHandler>
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::test_utils::MockApiClient;
    use coderex_mcp::elicitation::SamplingMessage;

    fn request(max_tokens: u32, hints: &[&str]) -> SamplingRequest {
        SamplingRequest {
            messages: vec![SamplingMessage {
                role: SamplingRole::User,
                content: SamplingContent::text("hello"),
            }],
            model_preferences: Some(ModelPreferences {
                hints: hints
                    .iter()
                    .map(|name| coderex_mcp::elicitation::ModelHint {
                        name: Some((*name).to_string()),
                    })
                    .collect(),
                cost_priority: None,
                speed_priority: None,
                intelligence_priority: None,
            }),
            max_tokens,
            system_prompt: Some("be brief".to_string()),
            include_context: None,
            temperature: None,
            stop_sequences: Vec::new(),
            metadata: None,
        }
    }

    /// Run `run` with `name` set to `value` (or unset), holding `ENV_LOCK`.
    ///
    /// Only tests that read the process environment use this, so no other
    /// test observes the temporary value.
    #[allow(unsafe_code)] // SAFETY: the environment mutation is serialised by ENV_LOCK.
    fn with_env<T>(name: &str, value: Option<&str>, run: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(name);
        // SAFETY: ENV_LOCK is held and no other test mutates this variable.
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let result = run();
        // SAFETY: as above; ENV_LOCK is still held.
        unsafe {
            match previous {
                Some(previous) => std::env::set_var(name, previous),
                None => std::env::remove_var(name),
            }
        }
        result
    }

    #[test]
    fn sampling_is_refused_when_the_opt_in_is_absent() {
        with_env(SAMPLING_OPT_IN_ENV, None, || {
            assert!(!enabled_from_env());
            assert!(handler_from_env(Box::new(MockApiClient::new()), ModelId::sonnet()).is_none());
        });
    }

    #[test]
    fn only_the_allow_value_enables_sampling() {
        with_env(SAMPLING_OPT_IN_ENV, Some("allow"), || {
            assert!(enabled_from_env());
        });
        with_env(SAMPLING_OPT_IN_ENV, Some("yes"), || {
            assert!(!enabled_from_env());
        });
    }

    #[test]
    fn the_token_ceiling_is_overridable_and_never_zero() {
        with_env(MAX_TOKENS_ENV, None, || {
            assert_eq!(max_tokens_ceiling_from_env(), DEFAULT_MAX_TOKENS);
        });
        with_env(MAX_TOKENS_ENV, Some("128"), || {
            assert_eq!(max_tokens_ceiling_from_env(), 128);
        });
        with_env(MAX_TOKENS_ENV, Some("0"), || {
            assert_eq!(max_tokens_ceiling_from_env(), DEFAULT_MAX_TOKENS);
        });
    }

    #[tokio::test]
    async fn the_handler_clamps_the_requested_max_tokens() {
        let mock = MockApiClient::new();
        mock.enqueue_response("answer", 1, 1);
        let handler =
            CliSamplingHandler::with_ceiling(Box::new(mock.clone()), ModelId::sonnet(), 256);

        let result = handler
            .sample(&request(5_000, &[]))
            .await
            .expect("the mock answers");
        assert_eq!(result.role, SamplingRole::Assistant);
        assert_eq!(result.content.as_text(), Some("answer"));

        let log = mock.call_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].3, 256, "the server's 5000 is clamped to the ceiling");
        assert_eq!(log[0].0, "be brief");
        assert_eq!(log[0].2, ModelId::sonnet().0);
    }

    #[tokio::test]
    async fn a_known_hint_picks_the_model_and_an_unknown_one_falls_back() {
        let mock = MockApiClient::new();
        mock.enqueue_response("first", 1, 1);
        mock.enqueue_response("second", 1, 1);
        let handler =
            CliSamplingHandler::with_ceiling(Box::new(mock.clone()), ModelId::sonnet(), 1024);

        handler
            .sample(&request(32, &["not-a-model", "haiku"]))
            .await
            .expect("the mock answers");
        handler
            .sample(&request(32, &["not-a-model"]))
            .await
            .expect("the mock answers");

        let log = mock.call_log();
        assert_eq!(log[0].2, ModelId::haiku().0);
        assert_eq!(log[1].2, ModelId::sonnet().0);
    }

    #[tokio::test]
    async fn an_api_error_is_returned_to_the_server() {
        let mock = MockApiClient::new();
        mock.enqueue_error("no route to host");
        let handler = CliSamplingHandler::with_ceiling(Box::new(mock), ModelId::sonnet(), 1024);

        let error = handler
            .sample(&request(32, &[]))
            .await
            .expect_err("an API failure is not a panic");
        assert_eq!(error.code, INTERNAL_ERROR);
        assert!(
            error.message.contains("no route to host"),
            "{}",
            error.message
        );
    }
}
