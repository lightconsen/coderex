//! Permission requests, hook-based short-circuit authorization,
//! pending request persistence, and orphaned recovery.

use crate::confirmation_bus::{ConfirmationBus, ConfirmationRequest, ConfirmationResponse};
use crate::permissions::{PermissionChecker, PermissionResult, ToolRuleExemptions};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Answers a tool's rule exemptions for one specific call.
///
/// The flags are per-*input*, not per-tool: the artifact tool refuses a
/// whole-tool allow rule for its mutating actions and honours it for its
/// reads. So the engine asks this with the call's own arguments; the tool and
/// the registry that know the tool's shape answer.
///
/// `coderex-tools` installs a registry-backed one through
/// `install_rule_exemptions`.
pub type RuleExemptionsResolver =
    dyn Fn(&str, &serde_json::Value) -> ToolRuleExemptions + Send + Sync;

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
    /// Optional per-call resolver for tool rule exemptions.
    ///
    /// Without one the checker's static table is used, which is all a caller
    /// that only knows a tool name can answer.
    rule_exemptions: Option<Box<RuleExemptionsResolver>>,
}

/// The denial to report when nobody answered.
///
/// `None` from the bus means the confirmation channel closed before a decision
/// arrived — not that the person refused. Inventing "denied by user" there
/// makes a vanished UI indistinguishable from a real refusal.
fn unanswered_ask(tool_name: &str) -> PermissionResult {
    PermissionResult::Deny {
        reason: format!(
            "tool '{tool_name}' needs approval, but the confirmation channel closed before an answer arrived"
        ),
    }
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
            rule_exemptions: None,
        }
    }

    /// Install a resolver that answers a tool's rule exemptions per call.
    ///
    /// The flags are per-input — see [`RuleExemptionsResolver`] — so the engine
    /// asks the resolver with the call's own arguments. While one is installed
    /// it replaces the checker's static table; without one the engine falls
    /// back to that table, which is the behaviour of every session that has not
    /// installed a resolver.
    pub fn set_rule_exemptions_resolver(&mut self, resolver: Box<RuleExemptionsResolver>) {
        self.rule_exemptions = Some(resolver);
    }

    /// The exemptions that apply to one call.
    ///
    /// Falls back to the checker's static table when no resolver is installed.
    fn exemptions_for(&self, tool_name: &str, arguments: &serde_json::Value) -> ToolRuleExemptions {
        match self.rule_exemptions.as_ref() {
            Some(resolve) => resolve(tool_name, arguments),
            None => self.checker.lock().unwrap().tool_exemptions(tool_name),
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

    /// Clone the interactive channel for another permission flow in the engine.
    pub(crate) fn confirmation_bus(&self) -> Option<ConfirmationBus> {
        self.confirmation_bus.clone()
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
        //
        // The tool's rule exemptions are a property of this call, not only of
        // the tool, so they are resolved from the call's own arguments. They
        // are resolved once and reused below: the same answer decides whether a
        // whole-tool allow rule is honoured and whether the session-wide
        // approval is offered or written.
        let arguments: serde_json::Value = serde_json::from_str(&request.arguments)
            .unwrap_or(serde_json::Value::Null);
        let exemptions = self.exemptions_for(tool_name, &arguments);
        let result = {
            let checker = self.checker.lock().unwrap();
            let path_ref = request.path.as_deref();
            checker.check_with_exemptions(tool_name, path_ref, Some(exemptions))
        };

        // Phase 2.5: shell security analysis
        //
        // The checker sees only the tool name and path. Enrich its generic
        // approval reason with the parsed command's destructive behavior, and
        // inspect every stage of a compound command rather than just the first.
        let shell_analysis = shell_security_in(tool_name, &request.arguments);
        let result = match result {
            PermissionResult::Ask { reason } => {
                let reason = shell_analysis
                    .as_ref()
                    .filter(|ast| ast.needs_warning())
                    .map_or(reason, crate::shell_security::BashAst::summary);
                PermissionResult::Ask { reason }
            }
            PermissionResult::AskForSession { reason } => {
                let reason = shell_analysis
                    .as_ref()
                    .filter(|ast| ast.needs_warning())
                    .map_or(reason, crate::shell_security::BashAst::summary);
                PermissionResult::AskForSession { reason }
            }
            other => other,
        };

        // Phase 2.6: privilege escalation
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
        //
        // A tool that suppresses the "always allow" choice is offered the
        // one-shot approval only.
        let suppresses_always_allow = exemptions.suppresses_always_allow_rule;
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
                        allow_session: !suppresses_always_allow
                            && matches!(
                                result,
                                PermissionResult::Ask { .. }
                                    | PermissionResult::AskForSession { .. }
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
                            // The flag's second job: a suppressed tool does not
                            // get its approval written as a session-wide one.
                            // The prompt never offers it, so this only bites a
                            // caller that asks for it anyway — and it approves
                            // this call either way, it just does not remember.
                            if !suppresses_always_allow {
                                let mut checker = self.checker.lock().unwrap();
                                checker.record_session_approval(tool_name.to_string(), true);
                            }
                            PermissionResult::Allow
                        }
                        Some(ConfirmationResponse::Denied) => PermissionResult::Deny {
                            reason: format!("tool '{}' denied by user", tool_name),
                        },
                        // A closed channel is not a decision. The elevation
                        // path already tells these apart (`abandon_pending_elevation`);
                        // this is the ordinary Ask path catching up, so a
                        // transcript or a headless caller is not told a person
                        // said no when nobody was asked.
                        None => unanswered_ask(tool_name),
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
            // Find the request to get the tool name and the call it was for:
            // a tool that suppresses the session-wide allow is not given one,
            // however the answer arrived.
            let found = self
                .pending
                .pending()
                .iter()
                .find(|r| r.id == request_id)
                .map(|req| {
                    (
                        req.tool_name.clone(),
                        serde_json::from_str(&req.arguments).unwrap_or(serde_json::Value::Null),
                    )
                });
            if let Some((tool, arguments)) = found {
                if !self.exemptions_for(&tool, &arguments).suppresses_always_allow_rule {
                    let mut checker = self.checker.lock().unwrap();
                    checker.record_session_approval(tool, true);
                }
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
                if classification == "safe" {
                    // Model classification is advisory, never proof. A static
                    // parse that finds destructive behavior wins over a model
                    // saying the same command is safe (or merely "unsafe").
                    if shell_security_in(&request.tool_name, &request.arguments)
                        .is_some_and(|ast| ast.is_dangerous())
                    {
                        Ok(None)
                    } else {
                        Ok(Some(PermissionResult::Allow))
                    }
                } else {
                    // caution, dangerous, or an unexpected answer → fall through
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

/// Parse the shell command carried by a shell tool call, if present.
///
/// Tool arguments in any other tool are data, not a command to analyze.
fn shell_security_in(tool_name: &str, arguments: &str) -> Option<crate::shell_security::BashAst> {
    if !tool_name.eq_ignore_ascii_case("bash") && !tool_name.eq_ignore_ascii_case("powershell") {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let command = parsed
        .get("command")
        .or_else(|| parsed.get("cmd"))
        .and_then(serde_json::Value::as_str)?;
    Some(crate::shell_security::BashAst::parse(command))
}

/// The privilege-changing program in a shell tool call, if any.
///
/// Only shell tools carry a command to inspect; every other tool has no
/// privilege question to answer.
fn privilege_escalation_in(tool_name: &str, arguments: &str) -> Option<String> {
    if !tool_name.eq_ignore_ascii_case("bash") && !tool_name.eq_ignore_ascii_case("powershell") {
        return None;
    }
    shell_security_in(tool_name, arguments)?.privilege_escalation()
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

    /// A missing answer is not a decision. The arm is defensive — the engine
    /// holds a bus clone, so today the channel cannot close mid-request — but
    /// the wording must not invent a refusal if it ever becomes reachable.
    #[test]
    fn an_unanswered_ask_is_not_reported_as_a_user_denial() {
        let result = unanswered_ask("bash");
        let PermissionResult::Deny { reason } = result else {
            panic!("an unanswered ask closes the operation");
        };
        assert!(reason.contains("closed"), "{reason}");
        assert!(!reason.contains("by user"), "a person was not asked: {reason}");
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

    /// Ask the bus for one request and hand back whether it offered the
    /// session-wide approval.
    async fn session_option_offered_for(
        tool: &str,
        exemptions: crate::permissions::ToolRuleExemptions,
    ) -> bool {
        use crate::confirmation_bus::{ConfirmationBus, ConfirmationResponse};

        let bus = ConfirmationBus::new(10);
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.set_tool_exemptions(tool, exemptions);
        let mut engine = PermissionEngine::new(checker);
        engine.set_confirmation_bus(bus.clone());

        let bus_clone = bus.clone();
        let owned = tool.to_string();
        let handle = tokio::spawn(async move {
            let (_request, result) = engine
                .evaluate(&owned, "{}".into(), None, HashMap::new())
                .await
                .unwrap();
            result
        });

        let req = bus_clone.next_request().await.unwrap();
        let offered = req.allow_session;
        bus_clone
            .respond(&req.id, ConfirmationResponse::Approved)
            .await;
        let _ = handle.await.unwrap();
        offered
    }

    /// A call whose exemptions come from a resolver: run it through the bus,
    /// answer it, and hand back what the prompt offered, the verdict, and what
    /// a fresh check of the same tool says afterwards — the last one shows
    /// whether the answer was written down as a session-wide approval.
    async fn one_bus_call(
        tool: &str,
        arguments: serde_json::Value,
        exemptions: crate::permissions::ToolRuleExemptions,
        resolver: Option<Box<RuleExemptionsResolver>>,
        response: ConfirmationResponse,
    ) -> (bool, PermissionResult, PermissionResult) {
        use crate::confirmation_bus::ConfirmationBus;

        let bus = ConfirmationBus::new(10);
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.set_tool_exemptions(tool, exemptions);
        let mut engine = PermissionEngine::new(checker);
        engine.set_confirmation_bus(bus.clone());
        if let Some(resolver) = resolver {
            engine.set_rule_exemptions_resolver(resolver);
        }

        let reply_bus = bus.clone();
        let answer = async move {
            let request = reply_bus.next_request().await.unwrap();
            let offered = request.allow_session;
            reply_bus.respond(&request.id, response).await;
            offered
        };
        let (evaluated, offered) = tokio::join!(
            engine.evaluate(tool, arguments.to_string(), None, HashMap::new()),
            answer
        );
        let verdict = evaluated.unwrap().1;
        let written = engine.checker().lock().unwrap().check(tool, None);
        (offered, verdict, written)
    }

    /// The flags are answered per call, not per tool: the same tool honours a
    /// whole-tool allow rule for a read and refuses it for a publish, and the
    /// refusal falls through to the ordinary default rather than to a prompt
    /// invented for the exemption.
    #[tokio::test]
    async fn a_resolver_answers_rule_exemptions_for_each_call() {
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker
            .add_rule(crate::permissions::PermissionRule {
                tool_name: "Write".into(),
                action: crate::permissions::PermissionAction::Allow,
                paths: Vec::new(),
                description: String::new(),
            })
            .unwrap();
        let mut engine = PermissionEngine::new(checker);
        engine.set_rule_exemptions_resolver(Box::new(|_tool, input| {
            if input.get("action").and_then(serde_json::Value::as_str) == Some("publish") {
                ToolRuleExemptions {
                    ignores_whole_tool_allow_rule: true,
                    ..ToolRuleExemptions::NONE
                }
            } else {
                ToolRuleExemptions::NONE
            }
        }));

        let (_request, result) = engine
            .evaluate(
                "Write",
                serde_json::json!({ "action": "read" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        assert!(
            matches!(result, PermissionResult::Allow),
            "a read carries no exemption, so the whole-tool allow rule must stand: {result:?}"
        );

        let (_request, result) = engine
            .evaluate(
                "Write",
                serde_json::json!({ "action": "publish" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        match result {
            PermissionResult::Ask { reason } => {
                assert!(
                    reason.contains("dangerous operation"),
                    "a refused allow rule falls to the ordinary default: {reason}"
                );
                assert!(
                    !reason.contains("not covered by whole-tool allow rules"),
                    "the exemption is not a prompt of its own: {reason}"
                );
            }
            other => panic!("a refused allow rule must not approve the call, got {other:?}"),
        }
    }

    /// Suppression is answered per call too, and it reaches the prompt: the
    /// same tool is offered the session-wide option for a read and not for a
    /// publish.
    #[tokio::test]
    async fn a_resolver_can_withhold_the_session_option_for_one_kind_of_call() {
        let resolver = || -> Box<RuleExemptionsResolver> {
            Box::new(|_tool, input| {
                if input.get("action").and_then(serde_json::Value::as_str) == Some("publish") {
                    ToolRuleExemptions {
                        suppresses_always_allow_rule: true,
                        ..ToolRuleExemptions::NONE
                    }
                } else {
                    ToolRuleExemptions::NONE
                }
            })
        };

        let (offered, _verdict, _written) = one_bus_call(
            "write",
            serde_json::json!({ "action": "read" }),
            ToolRuleExemptions::NONE,
            Some(resolver()),
            ConfirmationResponse::Approved,
        )
        .await;
        assert!(offered, "an ordinary call must still offer the session option");

        let (offered, _verdict, _written) = one_bus_call(
            "write",
            serde_json::json!({ "action": "publish" }),
            ToolRuleExemptions::NONE,
            Some(resolver()),
            ConfirmationResponse::Approved,
        )
        .await;
        assert!(
            !offered,
            "a suppressing call offered the session-wide option"
        );
    }

    /// The suppression's other job: a suppressed tool is not given the
    /// session-wide approval even when the answer asks for it. This call is
    /// still approved; the next one asks again.
    #[tokio::test]
    async fn a_suppressing_tool_gets_no_session_wide_approval_written() {
        let suppressing = ToolRuleExemptions {
            ignores_whole_tool_allow_rule: true,
            suppresses_always_allow_rule: true,
        };
        let (offered, verdict, written) = one_bus_call(
            "write",
            serde_json::json!({}),
            suppressing,
            None,
            ConfirmationResponse::ApproveSession,
        )
        .await;
        assert!(!offered);
        assert!(matches!(verdict, PermissionResult::Allow));
        assert!(
            matches!(written, PermissionResult::Ask { .. }),
            "the session-wide approval was written for a tool that suppresses it: {written:?}"
        );

        // The control: an ordinary tool's session approval is written, which is
        // what makes the check above mean something.
        let (offered, verdict, written) = one_bus_call(
            "write",
            serde_json::json!({}),
            ToolRuleExemptions::NONE,
            None,
            ConfirmationResponse::ApproveSession,
        )
        .await;
        assert!(offered);
        assert!(matches!(verdict, PermissionResult::Allow));
        assert!(
            matches!(written, PermissionResult::Allow),
            "an ordinary tool's session approval must still be written: {written:?}"
        );
    }

    /// The session-wide option is the default; a tool that suppresses it is
    /// offered the one-shot approval only.
    #[tokio::test]
    async fn suppresses_always_allow_rule_withholds_the_session_option() {
        assert!(
            session_option_offered_for("bash", crate::permissions::ToolRuleExemptions::NONE).await,
            "an ordinary tool must still offer the session option"
        );
        assert!(
            !session_option_offered_for(
                "bash",
                crate::permissions::ToolRuleExemptions {
                    ignores_whole_tool_allow_rule: true,
                    suppresses_always_allow_rule: true,
                },
            )
            .await,
            "the session option was offered for a tool that suppresses it"
        );
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

    #[tokio::test]
    async fn shell_analysis_enriches_approval_reason_and_covers_sequences() {
        let mut engine = PermissionEngine::new(PermissionChecker::new(PermissionMode::Default));
        let (_request, result) = engine
            .evaluate(
                "bash",
                serde_json::json!({ "command": "echo ok && rm -f /tmp/x" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        match result {
            PermissionResult::Ask { reason } => {
                assert!(reason.contains("Dangerous"), "{reason}");
                assert!(reason.contains("rm -f"), "{reason}");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[derive(Clone)]
    struct StaticClassifier(&'static str);

    #[async_trait]
    impl crate::api_client::ApiClient for StaticClassifier {
        fn clone_box(&self) -> Box<dyn crate::api_client::ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &crate::types::ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<crate::api_client::ApiResponse, crate::api_client::ApiError> {
            Ok(crate::api_client::ApiResponse {
                content: self.0.into(),
                tool_calls: Vec::new(),
                usage: crate::types::UsageSummary {
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
            _model: &crate::types::ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
        > {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn auto_classifier_cannot_override_dangerous_shell_analysis() {
        let hook = AutoClassifierHook::new(Box::new(StaticClassifier("safe")));
        let mut request = make_request("bash");
        request.arguments = serde_json::json!({ "command": "rm -rf /tmp/x" }).to_string();
        let result = hook.evaluate(&request, PermissionMode::Auto).await.unwrap();
        assert!(result.is_none(), "static danger must beat a safe verdict");
    }

    #[tokio::test]
    async fn auto_classifier_does_not_treat_unsafe_as_safe() {
        let hook = AutoClassifierHook::new(Box::new(StaticClassifier("unsafe")));
        let request = make_request("read");
        let result = hook.evaluate(&request, PermissionMode::Auto).await.unwrap();
        assert!(
            result.is_none(),
            "the substring 'safe' must not be a verdict"
        );
    }
}
