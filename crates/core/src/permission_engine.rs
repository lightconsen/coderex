//! Permission requests, hook-based short-circuit authorization,
//! pending request persistence, and orphaned recovery.

use crate::confirmation_bus::{ConfirmationBus, ConfirmationRequest, ConfirmationResponse};
use crate::permissions::{PermissionChecker, PermissionResult};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Errors from the permission request system.
#[derive(Debug, Error)]
pub enum PermissionRequestError {
    /// Failed to persist a pending request.
    #[error("failed to persist pending request: {0}")]
    PersistFailed(String),
    /// Failed to recover orphaned requests.
    #[error("failed to recover orphaned requests: {0}")]
    RecoveryFailed(String),
    /// Hook execution failed.
    #[error("permission hook failed: {0}")]
    HookError(String),
}

/// A request for permission to execute a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Unique request ID.
    pub id: String,
    /// Tool name being requested.
    pub tool_name: String,
    /// JSON-serialized arguments.
    pub arguments: String,
    /// Optional path the tool will operate on.
    pub path: Option<PathBuf>,
    /// Context metadata (session ID, turn number, etc.).
    pub context: HashMap<String, String>,
    /// When this request was created.
    pub created_at: DateTime<Utc>,
}

/// Response to a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionResponse {
    /// The user approved the operation.
    Approved,
    /// The user approved for the rest of the session.
    ApproveSession,
    /// The user denied the operation.
    Denied,
}

/// Trait for permission hooks that can short-circuit authorization.
///
/// Hooks run before the standard permission checker. If a hook returns
/// `Some(result)`, that result is used directly and the checker is skipped.
#[async_trait]
pub trait PermissionHook: Send + Sync {
    /// Evaluate a permission request.
    ///
    /// Returns `Some(result)` to short-circuit and bypass the checker,
    /// or `None` to let the checker decide.
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        mode: crate::permissions::PermissionMode,
    ) -> Result<Option<PermissionResult>, PermissionRequestError>;
}

/// Stores pending permission requests that await user response.
#[allow(missing_debug_implementations)]
pub struct PendingPermissionStore {
    pending: HashMap<String, PermissionRequest>,
    persist_path: Option<PathBuf>,
}

impl PendingPermissionStore {
    /// Create a new store, optionally persisting to the given file.
    #[must_use]
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        Self {
            pending: HashMap::new(),
            persist_path,
        }
    }

    /// Add a pending permission request.
    pub fn add(&mut self, request: PermissionRequest) {
        self.pending.insert(request.id.clone(), request);
    }

    /// Resolve a pending request and remove it from the store.
    pub fn resolve(&mut self, request_id: &str, _response: PermissionResponse) {
        self.pending.remove(request_id);
    }

    /// Get all currently pending requests.
    #[must_use]
    pub fn pending(&self) -> Vec<&PermissionRequest> {
        self.pending.values().collect()
    }

    /// Check if a specific request is still pending.
    #[must_use]
    pub fn is_pending(&self, request_id: &str) -> bool {
        self.pending.contains_key(request_id)
    }

    /// Persist all pending requests to disk.
    pub fn persist(&self) -> Result<(), PermissionRequestError> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(&self.pending.values().collect::<Vec<_>>())
            .map_err(|e| PermissionRequestError::PersistFailed(e.to_string()))?;
        std::fs::write(path, json)
            .map_err(|e| PermissionRequestError::PersistFailed(e.to_string()))?;
        Ok(())
    }

    /// Load pending requests from disk.
    pub fn load(&mut self) -> Result<(), PermissionRequestError> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let data = std::fs::read_to_string(path)
            .map_err(|e| PermissionRequestError::PersistFailed(e.to_string()))?;
        let requests: Vec<PermissionRequest> = serde_json::from_str(&data)
            .map_err(|e| PermissionRequestError::PersistFailed(e.to_string()))?;
        for req in requests {
            self.pending.insert(req.id.clone(), req);
        }
        Ok(())
    }

    /// Count of pending requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// Orchestrates permission evaluation: hooks → checker → confirmation bus → default.
#[allow(missing_debug_implementations)]
pub struct PermissionEngine {
    checker: Arc<std::sync::Mutex<PermissionChecker>>,
    hooks: Vec<Arc<dyn PermissionHook>>,
    pending: PendingPermissionStore,
    request_counter: u64,
    /// Optional confirmation bus for async user approval.
    confirmation_bus: Option<ConfirmationBus>,
}

impl PermissionEngine {
    /// Create a new permission engine with the given checker.
    #[must_use]
    pub fn new(checker: PermissionChecker) -> Self {
        Self {
            checker: Arc::new(std::sync::Mutex::new(checker)),
            hooks: Vec::new(),
            pending: PendingPermissionStore::new(None),
            request_counter: 0,
            confirmation_bus: None,
        }
    }

    /// Set the confirmation bus for async permission requests.
    ///
    /// When a bus is configured, `evaluate()` will send `Ask`/`AskForSession`
    /// results through the bus and await the user response before returning.
    pub fn set_confirmation_bus(&mut self, bus: ConfirmationBus) {
        self.confirmation_bus = Some(bus);
    }

    /// Remove the confirmation bus.
    pub fn clear_confirmation_bus(&mut self) {
        self.confirmation_bus = None;
    }

    /// Set the pending store persistence path.
    pub fn set_persist_path(&mut self, path: PathBuf) {
        self.pending = PendingPermissionStore::new(Some(path));
    }

    /// Register a permission hook.
    pub fn add_hook(&mut self, hook: Arc<dyn PermissionHook>) {
        self.hooks.push(hook);
    }

    /// Evaluate a permission request through the full pipeline.
    ///
    /// 1. Run hooks (first hook returning `Some` short-circuits)
    /// 2. Fall back to the permission checker
    /// 3. If result is `Ask`/`AskForSession` and a [`ConfirmationBus`] is
    ///    configured, send the request through the bus and await the response.
    pub async fn evaluate(
        &mut self,
        tool_name: &str,
        arguments: String,
        path: Option<PathBuf>,
        context: HashMap<String, String>,
    ) -> Result<(PermissionRequest, PermissionResult), PermissionRequestError> {
        let mode = {
            let checker = self.checker.lock().unwrap();
            checker.mode()
        };

        self.request_counter += 1;
        let id = format!("perm_{}", self.request_counter);

        let request = PermissionRequest {
            id: id.clone(),
            tool_name: tool_name.to_string(),
            arguments,
            path,
            context,
            created_at: Utc::now(),
        };

        // Phase 1: Hook short-circuit
        for hook in &self.hooks {
            if let Some(result) = hook.evaluate(&request, mode).await? {
                return Ok((request, result));
            }
        }

        // Phase 2: Permission checker
        let result = {
            let checker = self.checker.lock().unwrap();
            let path_ref = request.path.as_deref();
            checker.check(tool_name, path_ref)
        };

        // Phase 2.5: privilege escalation
        //
        // A command that runs as another user is a different question from
        // "allow this command?": approving it once still runs it as the caller,
        // or fails. Surface it as an elevation request so the caller can decide
        // about privilege explicitly — this is the only producer of
        // `PermissionResult::Elevated`, and without it the elevation flow
        // downstream is unreachable.
        let result = match result {
            PermissionResult::Ask { .. } | PermissionResult::AskForSession { .. } => {
                privilege_escalation_in(tool_name, &request.arguments).map_or(result, |program| {
                    PermissionResult::Elevated {
                        reason: format!("{program} runs the command as another user"),
                    }
                })
            }
            other => other,
        };

        // Phase 3: Confirmation bus (if configured and result needs user input)
        let result = match result {
            PermissionResult::Ask { ref reason }
            | PermissionResult::AskForSession { ref reason } => {
                if let Some(ref bus) = self.confirmation_bus {
                    let confirmation_req = ConfirmationRequest {
                        id: id.clone(),
                        tool_name: tool_name.to_string(),
                        arguments: request.arguments.clone(),
                        path: request.path.clone(),
                        reason: reason.clone(),
                        allow_session: matches!(
                            result,
                            PermissionResult::Ask { .. } | PermissionResult::AskForSession { .. }
                        ),
                        context: request.context.clone(),
                    };

                    // Store as pending before sending
                    self.pending.add(request.clone());
                    let _ = self.pending.persist();

                    let response = bus.request(confirmation_req).await;

                    // Resolve from pending store
                    self.pending.resolve(&id, PermissionResponse::Denied);
                    let _ = self.pending.persist();

                    match response {
                        Some(ConfirmationResponse::Approved) => PermissionResult::Allow,
                        Some(ConfirmationResponse::ApproveSession) => {
                            // Record session approval
                            {
                                let mut checker = self.checker.lock().unwrap();
                                checker.record_session_approval(tool_name.to_string(), true);
                            }
                            PermissionResult::Allow
                        }
                        Some(ConfirmationResponse::Denied) | None => PermissionResult::Deny {
                            reason: format!("tool '{}' denied by user", tool_name),
                        },
                    }
                } else {
                    result
                }
            }
            other => other,
        };

        Ok((request, result))
    }

    /// Respond to a pending permission request.
    pub fn respond(
        &mut self,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<(), PermissionRequestError> {
        // Record session approval if applicable
        if response == PermissionResponse::ApproveSession {
            // Find the request to get the tool name
            if let Some(req) = self.pending.pending().iter().find(|r| r.id == request_id) {
                let tool = req.tool_name.clone();
                let mut checker = self.checker.lock().unwrap();
                checker.record_session_approval(tool, true);
            }
        }

        self.pending.resolve(request_id, response);
        self.pending.persist()
    }

    /// Recover orphaned permission requests from disk.
    pub fn recover_orphaned(&mut self) -> Result<Vec<PermissionRequest>, PermissionRequestError> {
        self.pending.load()?;
        Ok(self.pending.pending().into_iter().cloned().collect())
    }

    /// Get the current pending request count.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.pending_count()
    }

    /// Get the permission checker (shared).
    pub fn checker(&self) -> Arc<std::sync::Mutex<PermissionChecker>> {
        Arc::clone(&self.checker)
    }
}

/// A stub hook that never short-circuits (lets the checker decide).
#[derive(Debug)]
pub struct NoOpPermissionHook;

/// A hook that generates AI-powered risk explanations for permission requests.
///
/// This hook does NOT short-circuit — it always returns `None` so the
/// permission checker decides. It calls the AI to explain the risks
/// of the tool call and logs the explanation for downstream use.
#[allow(missing_debug_implementations)]
pub struct ExplainPermissionHook {
    api_client: Box<dyn crate::api_client::ApiClient>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::types::EngineEvent>>,
}

impl ExplainPermissionHook {
    /// Create a new hook with the given API client.
    #[must_use]
    pub fn new(api_client: Box<dyn crate::api_client::ApiClient>) -> Self {
        Self {
            api_client,
            event_tx: None,
        }
    }

    /// Create a new hook with an event sender for emitting `EngineEvent::PermissionExplanation`.
    #[must_use]
    pub fn new_with_event_tx(
        api_client: Box<dyn crate::api_client::ApiClient>,
        event_tx: tokio::sync::mpsc::UnboundedSender<crate::types::EngineEvent>,
    ) -> Self {
        Self {
            api_client,
            event_tx: Some(event_tx),
        }
    }
}

#[async_trait]
impl PermissionHook for ExplainPermissionHook {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _mode: crate::permissions::PermissionMode,
    ) -> Result<Option<PermissionResult>, PermissionRequestError> {
        let explanation = crate::prompts::explain_permission(
            self.api_client.as_ref(),
            &request.tool_name,
            &request.arguments,
        )
        .await
        .unwrap_or_else(|e| format!("[explanation unavailable: {e}]"));

        // Emit an event so the UI can display the explanation.
        if let Some(ref tx) = self.event_tx {
            let _ = tx.send(crate::types::EngineEvent::PermissionExplanation {
                tool: request.tool_name.clone(),
                explanation: explanation.clone(),
            });
        }

        tracing::info!(
            tool = %request.tool_name,
            explanation = %explanation,
            "permission explanation generated"
        );

        // Always pass through — let the checker decide
        Ok(None)
    }
}

#[async_trait]
impl PermissionHook for NoOpPermissionHook {
    async fn evaluate(
        &self,
        _request: &PermissionRequest,
        _mode: crate::permissions::PermissionMode,
    ) -> Result<Option<PermissionResult>, PermissionRequestError> {
        Ok(None)
    }
}

/// A hook that auto-approves requests matching a tool name pattern.
#[derive(Debug)]
pub struct AutoApproveHook {
    tool_names: Vec<String>,
}

impl AutoApproveHook {
    /// Create a hook that auto-approves the given tool names.
    #[must_use]
    pub const fn for_tools(tool_names: Vec<String>) -> Self {
        Self { tool_names }
    }
}

#[async_trait]
impl PermissionHook for AutoApproveHook {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _mode: crate::permissions::PermissionMode,
    ) -> Result<Option<PermissionResult>, PermissionRequestError> {
        if self
            .tool_names
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&request.tool_name))
        {
            Ok(Some(PermissionResult::Allow))
        } else {
            Ok(None)
        }
    }
}

/// A hook that auto-denies requests when in background/agent mode.
#[derive(Debug)]
pub struct BackgroundAgentDenyHook;

#[async_trait]
impl PermissionHook for BackgroundAgentDenyHook {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _mode: crate::permissions::PermissionMode,
    ) -> Result<Option<PermissionResult>, PermissionRequestError> {
        if let Some(exec_mode) = request.context.get("execution_mode") {
            if exec_mode == "background" || exec_mode == "agent" {
                return Ok(Some(PermissionResult::Deny {
                    reason: format!(
                        "tool '{}' denied in background agent mode",
                        request.tool_name
                    ),
                }));
            }
        }
        Ok(None)
    }
}

/// A hook that uses a lightweight AI query to classify permission risk in Auto mode.
///
/// Only activates when the permission mode is [`PermissionMode::Auto`].
/// Sends the tool name and arguments to a lightweight model and classifies
/// the operation as "safe", "caution", or "dangerous".
///
/// - **safe** → returns `Allow` (short-circuit)
/// - **caution** or **dangerous** → returns `None` (fall through to checker)
#[allow(missing_debug_implementations)]
pub struct AutoClassifierHook {
    api_client: Box<dyn crate::api_client::ApiClient>,
}

impl AutoClassifierHook {
    /// Create a new auto-classifier hook with the given API client.
    #[must_use]
    pub fn new(api_client: Box<dyn crate::api_client::ApiClient>) -> Self {
        Self { api_client }
    }
}

/// System prompt for the auto-classifier.
const AUTO_CLASSIFIER_SYSTEM: &str = "You are a security classifier. Given a tool name and its arguments, \
     classify the risk level as exactly one word: safe, caution, or dangerous. \
     Respond with ONLY the classification word.\n\n\
     safe = read-only or harmless operations (read, grep, ls, web_search, etc.)\n\
     caution = mutations to non-sensitive files (edit src files, write tests, etc.)\n\
     dangerous = destructive or sensitive operations (rm, sudo, system paths, secrets)";

#[async_trait]
impl PermissionHook for AutoClassifierHook {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        mode: crate::permissions::PermissionMode,
    ) -> Result<Option<PermissionResult>, PermissionRequestError> {
        if mode != crate::permissions::PermissionMode::Auto {
            return Ok(None);
        }

        let user_prompt = format!(
            "Tool: {}\nArguments: {}\n\nClassify the risk level:",
            request.tool_name, request.arguments
        );

        let result = crate::orchestrator::run_lightweight_query(
            self.api_client.as_ref(),
            AUTO_CLASSIFIER_SYSTEM,
            &user_prompt,
            32,
        )
        .await;

        match result {
            Ok(query_result) => {
                let classification = query_result.content.trim().to_lowercase();
                tracing::debug!(
                    tool = %request.tool_name,
                    classification = %classification,
                    "auto-classifier result"
                );
                if classification.contains("safe") {
                    Ok(Some(PermissionResult::Allow))
                } else {
                    // caution or dangerous → fall through to checker
                    Ok(None)
                }
            }
            Err(e) => {
                tracing::warn!("auto-classifier query failed: {e}");
                // On failure, fall through to checker
                Ok(None)
            }
        }
    }
}

/// The privilege-changing program in a shell tool call, if any.
///
/// Only shell tools carry a command to inspect; every other tool has no
/// privilege question to answer.
fn privilege_escalation_in(tool_name: &str, arguments: &str) -> Option<String> {
    if !tool_name.eq_ignore_ascii_case("bash") && !tool_name.eq_ignore_ascii_case("powershell") {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let command = parsed
        .get("command")
        .or_else(|| parsed.get("cmd"))
        .and_then(serde_json::Value::as_str)?;
    crate::shell_security::BashAst::parse(command).privilege_escalation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{PermissionChecker, PermissionMode};
    use crate::tool_executor::ToolInfo;
    use crate::types::{ModelId, UsageSummary};

    fn make_request(tool: &str) -> PermissionRequest {
        PermissionRequest {
            id: "test_1".into(),
            tool_name: tool.into(),
            arguments: "{}".into(),
            path: None,
            context: HashMap::new(),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn noop_hook_passes_through() {
        let hook = NoOpPermissionHook;
        let request = make_request("bash");
        let result = hook
            .evaluate(&request, PermissionMode::Default)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn auto_approve_hook_approves_matching_tool() {
        let hook = AutoApproveHook::for_tools(vec!["read".into()]);
        let request = make_request("read");
        let result = hook
            .evaluate(&request, PermissionMode::Default)
            .await
            .unwrap();
        assert!(matches!(result, Some(PermissionResult::Allow)));

        // Non-matching tool should pass through
        let request2 = make_request("bash");
        let result2 = hook
            .evaluate(&request2, PermissionMode::Default)
            .await
            .unwrap();
        assert!(result2.is_none());
    }

    #[tokio::test]
    async fn background_agent_deny_hook_blocks_background() {
        let hook = BackgroundAgentDenyHook;
        let mut context = HashMap::new();
        context.insert("execution_mode".into(), "background".into());

        let request = PermissionRequest {
            id: "test_2".into(),
            tool_name: "bash".into(),
            arguments: "{}".into(),
            path: None,
            context,
            created_at: Utc::now(),
        };

        let result = hook
            .evaluate(&request, PermissionMode::Default)
            .await
            .unwrap();
        assert!(matches!(result, Some(PermissionResult::Deny { .. })));
    }

    #[test]
    fn pending_store_add_resolve() {
        let mut store = PendingPermissionStore::new(None);
        let req = make_request("bash");
        store.add(req);
        assert_eq!(store.pending_count(), 1);
        assert!(store.is_pending("test_1"));

        store.resolve("test_1", PermissionResponse::Approved);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn pending_store_persist_and_load() {
        let dir = std::env::temp_dir().join("coderex_perm_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pending.json");

        let mut store = PendingPermissionStore::new(Some(path.clone()));
        let req = make_request("write");
        store.add(req);
        store.persist().unwrap();

        let mut store2 = PendingPermissionStore::new(Some(path.clone()));
        store2.load().unwrap();
        assert!(store2.is_pending("test_1"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_store_load_nonexistent_is_ok() {
        let dir = std::env::temp_dir().join("coderex_perm_test2");
        let path = dir.join("nonexistent.json");
        let mut store = PendingPermissionStore::new(Some(path));
        store.load().unwrap();
        assert_eq!(store.pending_count(), 0);
    }

    #[tokio::test]
    async fn permission_engine_hook_short_circuits() {
        let checker = PermissionChecker::new(PermissionMode::Default);
        let mut engine = PermissionEngine::new(checker);
        engine.add_hook(Arc::new(AutoApproveHook::for_tools(vec!["read".into()])));

        let (request, result) = engine
            .evaluate("read", "{}".into(), None, HashMap::new())
            .await
            .unwrap();
        assert_eq!(request.tool_name, "read");
        assert!(matches!(result, PermissionResult::Allow));
    }

    #[tokio::test]
    async fn permission_engine_falls_through_to_checker() {
        let checker = PermissionChecker::new(PermissionMode::Default);
        let mut engine = PermissionEngine::new(checker);
        // No hooks — falls through to checker
        engine.add_hook(Arc::new(NoOpPermissionHook));

        let (request, result) = engine
            .evaluate("bash", "{}".into(), None, HashMap::new())
            .await
            .unwrap();
        assert_eq!(request.tool_name, "bash");
        // Default mode: dangerous tool requires approval
        assert!(matches!(result, PermissionResult::Ask { .. }));
    }

    #[test]
    fn orphaned_recovery_loads_pending() {
        let dir = std::env::temp_dir().join("coderex_perm_recovery");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recovery.json");

        // Create store, add request, persist
        let mut store = PendingPermissionStore::new(Some(path.clone()));
        store.add(make_request("edit"));
        store.persist().unwrap();
        drop(store);

        // Create new engine, recover orphaned
        let checker = PermissionChecker::new(PermissionMode::Default);
        let mut engine = PermissionEngine::new(checker);
        engine.set_persist_path(path.clone());
        engine.recover_orphaned().unwrap();
        assert_eq!(engine.pending_count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_checker_mode_change_via_engine() {
        let checker = PermissionChecker::new(PermissionMode::Default);
        let engine = PermissionEngine::new(checker);

        {
            let guard = engine.checker();
            let mut c = guard.lock().unwrap();
            c.set_mode(PermissionMode::BypassPermissions);
        }

        {
            let guard = engine.checker();
            let c = guard.lock().unwrap();
            assert!(matches!(c.mode(), PermissionMode::BypassPermissions));
        }
    }

    #[tokio::test]
    async fn explain_permission_hook_passes_through() {
        use crate::api_client::{ApiClient, ApiResponse, StreamEvent};
        use futures::Stream;
        use std::pin::Pin;

        #[derive(Clone)]
        struct MockApiClient;

        #[async_trait::async_trait]
        impl ApiClient for MockApiClient {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(self.clone())
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "This tool could modify system files.".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 100,
                        output_tokens: 20,
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
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                Box::pin(futures::stream::empty())
            }
        }

        let hook = ExplainPermissionHook::new(Box::new(MockApiClient));
        let request = make_request("Write");
        let result = hook
            .evaluate(&request, PermissionMode::Default)
            .await
            .unwrap();
        // Should always pass through (not short-circuit)
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn auto_classifier_allows_safe_in_auto_mode() {
        use crate::api_client::{ApiClient, ApiResponse, StreamEvent};
        use futures::Stream;
        use std::pin::Pin;

        #[derive(Clone)]
        struct SafeClassifier;

        #[async_trait::async_trait]
        impl ApiClient for SafeClassifier {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(self.clone())
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "safe".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 50,
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
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                Box::pin(futures::stream::empty())
            }
        }

        let hook = AutoClassifierHook::new(Box::new(SafeClassifier));
        let request = make_request("read");
        let result = hook.evaluate(&request, PermissionMode::Auto).await.unwrap();
        assert!(matches!(result, Some(PermissionResult::Allow)));
    }

    #[tokio::test]
    async fn auto_classifier_falls_through_for_caution() {
        use crate::api_client::{ApiClient, ApiResponse, StreamEvent};
        use crate::types::UsageSummary;
        use futures::Stream;
        use std::pin::Pin;

        #[derive(Clone)]
        struct CautionClassifier;

        #[async_trait::async_trait]
        impl ApiClient for CautionClassifier {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(self.clone())
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "caution".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 50,
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
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                Box::pin(futures::stream::empty())
            }
        }

        let hook = AutoClassifierHook::new(Box::new(CautionClassifier));
        let request = make_request("edit");
        let result = hook.evaluate(&request, PermissionMode::Auto).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn auto_classifier_passes_through_in_non_auto_mode() {
        use crate::api_client::{ApiClient, ApiResponse, StreamEvent};
        use futures::Stream;
        use std::pin::Pin;

        #[derive(Clone)]
        struct NeverCalled;

        #[async_trait::async_trait]
        impl ApiClient for NeverCalled {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(self.clone())
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                panic!("API should not be called in non-auto mode")
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[ToolInfo],
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                Box::pin(futures::stream::empty())
            }
        }

        let hook = AutoClassifierHook::new(Box::new(NeverCalled));
        let request = make_request("read");
        let result = hook
            .evaluate(&request, PermissionMode::Default)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn confirmation_bus_approval_converts_ask_to_allow() {
        use crate::confirmation_bus::{ConfirmationBus, ConfirmationResponse};
        use std::collections::HashMap;

        let bus = ConfirmationBus::new(10);
        let checker = PermissionChecker::new(PermissionMode::Default);
        let mut engine = PermissionEngine::new(checker);
        engine.set_confirmation_bus(bus.clone());

        let bus_clone = bus.clone();
        let handle = tokio::spawn(async move {
            let (_request, result) = engine
                .evaluate("bash", "{}".into(), None, HashMap::new())
                .await
                .unwrap();
            result
        });

        let req = bus_clone.next_request().await.unwrap();
        assert_eq!(req.tool_name, "bash");
        bus_clone
            .respond(&req.id, ConfirmationResponse::Approved)
            .await;

        let result = handle.await.unwrap();
        assert!(matches!(result, PermissionResult::Allow));
    }

    #[tokio::test]
    async fn confirmation_bus_deny_converts_ask_to_deny() {
        use crate::confirmation_bus::{ConfirmationBus, ConfirmationResponse};
        use std::collections::HashMap;

        let bus = ConfirmationBus::new(10);
        let checker = PermissionChecker::new(PermissionMode::Default);
        let mut engine = PermissionEngine::new(checker);
        engine.set_confirmation_bus(bus.clone());

        let bus_clone = bus.clone();
        let handle = tokio::spawn(async move {
            let (_request, result) = engine
                .evaluate("bash", "{}".into(), None, HashMap::new())
                .await
                .unwrap();
            result
        });

        let req = bus_clone.next_request().await.unwrap();
        bus_clone
            .respond(&req.id, ConfirmationResponse::Denied)
            .await;

        let result = handle.await.unwrap();
        assert!(matches!(result, PermissionResult::Deny { .. }));
    }

    #[tokio::test]
    async fn confirmation_bus_session_approval_caches() {
        use crate::confirmation_bus::{ConfirmationBus, ConfirmationResponse};
        use std::collections::HashMap;

        let bus = ConfirmationBus::new(10);
        let checker = PermissionChecker::new(PermissionMode::Default);
        let mut engine = PermissionEngine::new(checker);
        engine.set_confirmation_bus(bus.clone());

        // First request: approve for session
        let bus_clone = bus.clone();
        let handle = tokio::spawn(async move {
            let (_request, result) = engine
                .evaluate("write", "{}".into(), None, HashMap::new())
                .await
                .unwrap();
            result
        });

        let req = bus_clone.next_request().await.unwrap();
        bus_clone
            .respond(&req.id, ConfirmationResponse::ApproveSession)
            .await;

        let result = handle.await.unwrap();
        assert!(matches!(result, PermissionResult::Allow));

        // Second request: should be auto-approved via session cache
        // Note: engine was moved into the spawn, so we test via a new engine
        // with the same checker (session approvals are in the checker)
    }

    /// The elevation flow downstream is only reachable if something produces
    /// `PermissionResult::Elevated`. A command that runs as another user is the
    /// case the variant exists for.
    #[tokio::test]
    async fn sudo_command_becomes_an_elevation_request() {
        let mut engine = PermissionEngine::new(PermissionChecker::new(PermissionMode::Default));
        let (_request, result) = engine
            .evaluate(
                "bash",
                serde_json::json!({ "command": "sudo rm -rf /tmp/x" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        match result {
            PermissionResult::Elevated { reason } => {
                assert!(reason.contains("sudo"), "{reason}");
            }
            other => panic!("expected Elevated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn absolute_path_to_sudo_is_still_detected() {
        let mut engine = PermissionEngine::new(PermissionChecker::new(PermissionMode::Default));
        let (_request, result) = engine
            .evaluate(
                "bash",
                serde_json::json!({ "command": "/usr/bin/su -c 'id'" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        assert!(matches!(result, PermissionResult::Elevated { .. }));
    }

    /// An ordinary dangerous command keeps its plain approval question — only
    /// privilege changes are a different question.
    #[tokio::test]
    async fn ordinary_bash_command_still_asks() {
        let mut engine = PermissionEngine::new(PermissionChecker::new(PermissionMode::Default));
        let (_request, result) = engine
            .evaluate(
                "bash",
                serde_json::json!({ "command": "rm -rf /tmp/x" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        assert!(
            matches!(result, PermissionResult::Ask { .. }),
            "expected Ask, got {result:?}"
        );
    }

    /// A tool with no command has no privilege question to answer.
    #[tokio::test]
    async fn non_shell_tool_never_elevates() {
        let mut engine = PermissionEngine::new(PermissionChecker::new(PermissionMode::Default));
        let (_request, result) = engine
            .evaluate(
                "write",
                serde_json::json!({ "command": "sudo rm -rf /" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        assert!(
            !matches!(result, PermissionResult::Elevated { .. }),
            "only shell tools can elevate, got {result:?}"
        );
    }
}
