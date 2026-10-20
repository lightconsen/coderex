//! The session's side of the hook action kinds.
//!
//! Three hook kinds — `prompt`, `agent`, `mcp_tool` — need something the hooks
//! crate cannot reach: a model client and the tool registry. The session owns
//! both, so it installs this on its [`HookRegistry`](coderex_hooks::HookRegistry).
//! Without it those kinds still load and then **fail**, which on a gating event
//! denies: a gate nobody can answer must not read as an allow.
//!
//! This is the same shape as the rule-exemption resolver
//! (`coderex_tools::install_rule_exemptions`): the crate that owns the decision
//! asks, and the session that owns the capability answers.

use std::sync::Arc;

use coderex_core::api_client::ApiClient;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::types::ModelId;
use coderex_hooks::{
    ActionPrompt, HookActionRunner, HookError, HookRequestError, HookRequestHandler,
    METHOD_NOT_FOUND,
};

/// System prompt for an `agent` hook.
///
/// The reference describes the kind as an "agentic verifier", so the framing is
/// "judge whether this is acceptable", not "do the work" — the hook exists to
/// answer a question about the call it was handed.
const AGENT_SYSTEM: &str = "\
You are verifying a pending action on behalf of a coding agent.\n\n\
Answer with ONLY a JSON object, on one line:\n\
{\"action\":\"continue\"} to allow it, or {\"action\":\"stop\"} to refuse it.\n\
Refuse anything destructive, anything that reaches outside the working\n\
directory, and anything whose input does not match what the caller described.";

/// How many tokens a hook's answer may take.
///
/// An answer is a small JSON object. A hook that needs more than this is not
/// answering the question it was asked, and a larger budget would only buy a
/// longer non-answer.
const ANSWER_TOKENS: u32 = 512;

/// What the runner needs from the tool layer.
///
/// A trait rather than the dispatcher itself so the mapping can be tested
/// without standing up a registry and a sandbox.
#[async_trait::async_trait]
pub trait HookToolCaller: Send + Sync {
    /// Call a tool by its registered name, and return its output.
    async fn call(&self, name: &str, input: &serde_json::Value) -> Result<String, String>;
}

#[async_trait::async_trait]
impl HookToolCaller for coderex_tools::RegistryToolDispatcher {
    async fn call(&self, name: &str, input: &serde_json::Value) -> Result<String, String> {
        use coderex_core::tool_executor::ToolExecutor as _;

        // The call id is a label, not a correlation: nothing is waiting on a
        // matching result, because the hook consumes the text directly.
        let outcome = self.execute("hook_action", name, input).await;
        if outcome.result.is_error {
            // A tool that answered with an error did not answer the hook.
            Err(outcome.result.output)
        } else {
            Ok(outcome.result.output)
        }
    }
}

/// Answers the hook kinds that need a client.
pub struct SessionHookActionRunner {
    api: Box<dyn ApiClient>,
    tools: Arc<dyn HookToolCaller>,
    /// The session's model, used when a `prompt` hook names none.
    default_model: ModelId,
}

impl SessionHookActionRunner {
    /// Build the runner over the session's clients.
    #[must_use]
    pub fn new(
        api: Box<dyn ApiClient>,
        tools: Arc<dyn HookToolCaller>,
        default_model: ModelId,
    ) -> Self {
        Self {
            api,
            tools,
            default_model,
        }
    }
}

impl std::fmt::Debug for SessionHookActionRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHookActionRunner")
            .field("default_model", &self.default_model.0)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl HookActionRunner for SessionHookActionRunner {
    async fn answer(&self, request: &ActionPrompt) -> Result<String, HookError> {
        let model = request.model.as_deref().map_or_else(
            || self.default_model.clone(),
            |named| ModelId(named.to_string()),
        );
        let system = if request.agent { AGENT_SYSTEM } else { "" };

        let answer = coderex_core::orchestrator::run_side_query(
            self.api.as_ref(),
            system,
            &request.prompt,
            &model,
            ANSWER_TOKENS,
        )
        .await
        .map_err(|error| HookError::ActionFailed(error.to_string()))?;

        Ok(answer.content)
    }

    async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<String, HookError> {
        // The qualified name is what the registry knows the tool by; the server
        // and tool are how the hook's configuration names it.
        let name = format!("mcp__{server}__{tool}");
        self.tools
            .call(&name, input)
            .await
            .map_err(HookError::ActionFailed)
    }
}

/// Answers the requests a streaming hook asks of the session while it runs.
///
/// A `command` hook configured with `stream: true` may emit
/// `{"type":"request","id":N,"method":"..."}` and then wait for an answer on
/// its stdin. This supplies the two read-only questions such a hook may ask:
///
/// - `"context"` — `{"cwd", "session_id", "model", "permission_mode"}`, where
///   the mode is its display string (`"default"`, `"plan"`, ...).
/// - `"permission_mode"` — just that mode string.
///
/// Every other method is refused with [`METHOD_NOT_FOUND`] (`-32601`) and a
/// message naming it. The surface is deliberately read-only: it never exposes
/// environment variables, credentials, or the settings document, and it never
/// mutates session state.
///
/// The permission checker is the session's shared one, so a mode change made
/// elsewhere is visible here immediately rather than being frozen at install
/// time.
pub struct SessionHookRequestHandler {
    cwd: std::path::PathBuf,
    session_id: String,
    model: ModelId,
    permission_checker: Arc<std::sync::Mutex<PermissionChecker>>,
}

impl SessionHookRequestHandler {
    /// Build the handler over the session's facts.
    ///
    /// `permission_checker` must be the session's shared checker
    /// (`Engine::permission_checker()`), not a snapshot: a copy would answer
    /// with the mode as it was at install time.
    #[must_use]
    pub fn new(
        cwd: impl Into<std::path::PathBuf>,
        session_id: impl Into<String>,
        model: ModelId,
        permission_checker: Arc<std::sync::Mutex<PermissionChecker>>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            session_id: session_id.into(),
            model,
            permission_checker,
        }
    }

    /// The mode the shared checker is in right now.
    fn permission_mode(&self) -> PermissionMode {
        // A poisoned lock is still readable; refusing to answer would leave
        // the hook waiting for a fact the session can still provide.
        self.permission_checker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mode()
    }
}

impl std::fmt::Debug for SessionHookRequestHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHookRequestHandler")
            .field("cwd", &self.cwd)
            .field("session_id", &self.session_id)
            .field("model", &self.model.0)
            .finish_non_exhaustive()
    }
}

impl HookRequestHandler for SessionHookRequestHandler {
    fn handle(
        &self,
        method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, HookRequestError> {
        match method {
            "context" => Ok(serde_json::json!({
                "cwd": self.cwd.to_string_lossy(),
                "session_id": self.session_id.as_str(),
                "model": self.model.0.as_str(),
                "permission_mode": self.permission_mode().to_string(),
            })),
            "permission_mode" => Ok(serde_json::Value::String(
                self.permission_mode().to_string(),
            )),
            other => Err(HookRequestError {
                code: METHOD_NOT_FOUND,
                message: format!("the session does not provide '{other}'"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::api_client::{ApiError, ApiResponse, StreamEvent};
    use coderex_core::tool_executor::ToolInfo;
    use coderex_core::types::UsageSummary;
    use coderex_hooks::{ActionPrompt, HookActionRunner};

    /// Records what it was asked and answers with fixed text.
    struct RecordingApi {
        seen: Arc<std::sync::Mutex<Vec<(String, String, ModelId)>>>,
        answer: &'static str,
    }

    #[async_trait::async_trait]
    impl ApiClient for RecordingApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                seen: Arc::clone(&self.seen),
                answer: self.answer,
            })
        }

        async fn create_message(
            &self,
            system: &str,
            messages: &[serde_json::Value],
            model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            self.seen.lock().unwrap().push((
                system.to_string(),
                messages.iter().map(ToString::to_string).collect::<String>(),
                model.clone(),
            ));
            Ok(ApiResponse {
                content: self.answer.into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
    }

    /// Records the tool call and returns whatever it was told to.
    struct FakeTools {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        result: Result<&'static str, &'static str>,
    }

    #[async_trait::async_trait]
    impl HookToolCaller for FakeTools {
        async fn call(&self, name: &str, _input: &serde_json::Value) -> Result<String, String> {
            self.seen.lock().unwrap().push(name.to_string());
            match self.result {
                Ok(text) => Ok(text.to_string()),
                Err(text) => Err(text.to_string()),
            }
        }
    }

    fn runner(
        seen: Arc<std::sync::Mutex<Vec<(String, String, ModelId)>>>,
        tools: Arc<dyn HookToolCaller>,
    ) -> SessionHookActionRunner {
        SessionHookActionRunner::new(
            Box::new(RecordingApi {
                seen,
                answer: "{\"action\":\"continue\"}",
            }),
            tools,
            ModelId::sonnet(),
        )
    }

    fn tool_stub() -> Arc<dyn HookToolCaller> {
        Arc::new(FakeTools {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            result: Ok("{}"),
        })
    }

    #[tokio::test]
    async fn a_prompt_hook_asks_the_session_model_and_returns_its_text() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = runner(Arc::clone(&seen), tool_stub());

        let answer = runner
            .answer(&ActionPrompt {
                prompt: "is this ok?".into(),
                model: None,
                agent: false,
            })
            .await
            .unwrap();

        assert_eq!(answer, "{\"action\":\"continue\"}");
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].2,
            ModelId::sonnet(),
            "the session's model, not one invented here"
        );
        assert!(
            seen[0].1.contains("is this ok?"),
            "the prompt did not reach the model"
        );
        drop(seen);
    }

    /// A `prompt` hook may name a model, and that name wins.
    #[tokio::test]
    async fn a_named_model_is_used_instead_of_the_session_default() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = runner(Arc::clone(&seen), tool_stub());

        runner
            .answer(&ActionPrompt {
                prompt: "check".into(),
                model: Some("claude-opus-4-6".into()),
                agent: false,
            })
            .await
            .unwrap();

        assert_eq!(seen.lock().unwrap()[0].2, ModelId("claude-opus-4-6".into()));
    }

    /// The `agent` framing is a verifier: it carries a system prompt, and a
    /// plain `prompt` hook does not.
    #[tokio::test]
    async fn the_agent_framing_carries_a_verifier_system_prompt() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = runner(Arc::clone(&seen), tool_stub());

        runner
            .answer(&ActionPrompt {
                prompt: "verify".into(),
                model: Some("claude-haiku-4-5-20251001".into()),
                agent: true,
            })
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        assert!(
            seen[0].0.contains("verifying"),
            "an agent hook should be framed as a verifier"
        );
        drop(seen);
    }

    /// A hook names a server and a tool; the registry knows the qualified name.
    #[tokio::test]
    async fn an_mcp_tool_hook_calls_the_qualified_name() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools: Arc<dyn HookToolCaller> = Arc::new(FakeTools {
            seen: Arc::clone(&seen),
            result: Ok("{\"action\":\"stop\"}"),
        });
        let runner = runner(Arc::new(std::sync::Mutex::new(Vec::new())), tools);

        let answer = runner
            .call_mcp_tool("policy", "check", &serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(answer, "{\"action\":\"stop\"}");
        assert_eq!(seen.lock().unwrap()[0], "mcp__policy__check");
    }

    /// A tool that answered with an error did not answer the hook.
    #[tokio::test]
    async fn a_tool_error_is_a_failed_hook() {
        let tools: Arc<dyn HookToolCaller> = Arc::new(FakeTools {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            result: Err("no such server"),
        });
        let runner = runner(Arc::new(std::sync::Mutex::new(Vec::new())), tools);

        let result = runner
            .call_mcp_tool("gone", "check", &serde_json::json!({}))
            .await;

        assert!(matches!(result, Err(HookError::ActionFailed(_))));
    }

    fn request_handler(
        mode: PermissionMode,
    ) -> (
        SessionHookRequestHandler,
        Arc<std::sync::Mutex<PermissionChecker>>,
    ) {
        let checker = Arc::new(std::sync::Mutex::new(PermissionChecker::new(mode)));
        let handler = SessionHookRequestHandler::new(
            "/work/project",
            "session-1",
            ModelId("claude-sonnet-4-5".into()),
            Arc::clone(&checker),
        );
        (handler, checker)
    }

    /// `context` reports exactly the four session facts — and nothing that
    /// could leak the environment, credentials, or the settings document.
    #[test]
    fn context_reports_the_session_facts() {
        let (handler, _) = request_handler(PermissionMode::Plan);

        let answer = handler.handle("context", None).unwrap();

        assert_eq!(answer["cwd"], "/work/project");
        assert_eq!(answer["session_id"], "session-1");
        assert_eq!(answer["model"], "claude-sonnet-4-5");
        assert_eq!(answer["permission_mode"], "plan");
        assert_eq!(
            answer.as_object().unwrap().len(),
            4,
            "context must expose only the documented facts"
        );
    }

    /// `permission_mode` answers the mode as its display string.
    #[test]
    fn permission_mode_reports_the_mode_string() {
        let (handler, _) = request_handler(PermissionMode::AcceptEdits);

        assert_eq!(
            handler.handle("permission_mode", None).unwrap(),
            serde_json::json!("accept-edits")
        );
    }

    /// The handler holds the session's *shared* checker, so later mode changes
    /// are visible without rebuilding it.
    #[test]
    fn permission_mode_reads_the_shared_checker() {
        let (handler, checker) = request_handler(PermissionMode::Default);
        assert_eq!(
            handler.handle("permission_mode", None).unwrap(),
            serde_json::json!("default")
        );

        checker.lock().unwrap().set_mode(PermissionMode::Plan);

        assert_eq!(
            handler.handle("permission_mode", None).unwrap(),
            serde_json::json!("plan")
        );
    }

    /// An unprovided method is refused, never ignored: the hook is waiting.
    #[test]
    fn an_unknown_method_is_method_not_found() {
        let (handler, _) = request_handler(PermissionMode::Default);

        let error = handler.handle("environment", None).unwrap_err();

        assert_eq!(error.code, METHOD_NOT_FOUND);
        assert_eq!(error.code, -32601);
        assert_eq!(error.message, "the session does not provide 'environment'");
    }
}
