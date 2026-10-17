//! Tool registry — manages baseline, feature-gated, and MCP tools with
//! context-based filtering and deny rules.

use crate::context::ExecutionContext;
use crate::tool::{BoxedTool, InterruptBehavior, Tool, ToolDescriptor, ToolTier};
use coderex_core::permissions::ToolRuleExemptions;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Errors from the tool registry.
#[derive(Debug, Error)]
pub enum ToolNotFoundError {
    /// The requested tool was not found in the registry.
    #[error("tool '{0}' not found")]
    ToolNotFound(String),
}

/// A deny rule that blocks tool execution based on name or path patterns.
#[derive(Debug, Clone)]
pub struct ToolDenyRule {
    /// Tool names this rule applies to (empty = all tools).
    pub tool_names: Vec<String>,
    /// Glob patterns that, if matched, deny execution.
    pub path_globs: Vec<String>,
    /// Whether this rule applies in plan mode.
    pub applies_in_plan_mode: bool,
}

impl ToolDenyRule {
    /// Create a rule that denies all tools matching any of the given names.
    #[must_use]
    pub const fn for_tools(tool_names: Vec<String>) -> Self {
        Self {
            tool_names,
            path_globs: Vec::new(),
            applies_in_plan_mode: false,
        }
    }

    /// Create a rule that denies tools operating on files matching the glob.
    #[must_use]
    pub const fn for_paths(path_globs: Vec<String>) -> Self {
        Self {
            tool_names: Vec::new(),
            path_globs,
            applies_in_plan_mode: false,
        }
    }

    /// Check if this rule applies to a given tool name in plan mode.
    ///
    /// A path pattern is a glob, matched against the whole path: `**/secrets/**`
    /// means that, and `/etc/passwd` means that file. The previous version
    /// looked for the pattern as a *substring*, so `/etc/**` — which is what a
    /// rule would be written as — matched nothing, and a deny rule that matches
    /// nothing denies nothing.
    pub fn matches(&self, tool_name: &str, path_args: &[PathBuf], plan_mode: bool) -> bool {
        if plan_mode && !self.applies_in_plan_mode {
            return false;
        }
        if !self.tool_names.is_empty() {
            return self
                .tool_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(tool_name));
        }

        if self.path_globs.is_empty() {
            return false;
        }

        for pattern in &self.path_globs {
            let Ok(glob) = globset::Glob::new(pattern) else {
                // A rule whose pattern will not compile can never match, which
                // is the same as not being there — say so rather than let it
                // look like protection.
                tracing::warn!(
                    pattern = %pattern,
                    "deny rule has a pattern that is not a glob; it cannot match anything"
                );
                continue;
            };
            let matcher = glob.compile_matcher();
            if path_args.iter().any(|path| matcher.is_match(path)) {
                return true;
            }
        }
        false
    }
}

/// Manages tool registration and lookup by context.
#[allow(missing_debug_implementations)]
pub struct ToolRegistry {
    baseline: HashMap<String, BoxedTool>,
    gated: HashMap<String, BoxedTool>,
    dynamic_mcp: RwLock<HashMap<String, BoxedTool>>,
    context_allowlists: HashMap<ExecutionContext, HashSet<String>>,
    deny_rules: Vec<ToolDenyRule>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Create a new empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            baseline: HashMap::new(),
            gated: HashMap::new(),
            dynamic_mcp: RwLock::new(HashMap::new()),
            context_allowlists: HashMap::new(),
            deny_rules: Vec::new(),
        }
    }

    /// Register a tool in the appropriate tier store.
    pub fn register(&mut self, tool: BoxedTool) {
        let descriptor = tool.descriptor();
        match descriptor.tier {
            ToolTier::Baseline => {
                self.baseline.insert(descriptor.name, tool);
            }
            ToolTier::FeatureGated => {
                self.gated.insert(descriptor.name, tool);
            }
            ToolTier::DynamicMcp => {
                self.dynamic_mcp.write().insert(descriptor.name, tool);
            }
        }
    }

    /// Register a dynamically discovered tool.
    ///
    /// Unlike [`register`](Self::register), this takes `&self`: dynamic tools
    /// (MCP servers) appear while the session is already running, after the
    /// registry has been shared.
    pub fn register_dynamic(&self, tool: BoxedTool) {
        let descriptor = tool.descriptor();
        self.dynamic_mcp.write().insert(descriptor.name, tool);
    }

    /// Add a deny rule to the registry.
    pub fn add_deny_rule(&mut self, rule: ToolDenyRule) {
        self.deny_rules.push(rule);
    }

    /// Set the allowlist for an execution context.
    ///
    /// Tools not in the allowlist will not be available in that context.
    pub fn set_context_allowlist(&mut self, context: ExecutionContext, tools: HashSet<String>) {
        self.context_allowlists.insert(context, tools);
    }

    /// Look up a tool by name across baseline and gated tiers.
    ///
    /// MCP tools are not returned here (they require the read lock).
    /// Use `get_tool_arc` for a lookup that includes MCP tools.
    pub fn get_tool(&self, name: &str) -> Result<&dyn Tool, ToolNotFoundError> {
        if let Some(t) = self.baseline.get(name) {
            return Ok(t.as_ref());
        }
        if let Some(t) = self.gated.get(name) {
            return Ok(t.as_ref());
        }
        Err(ToolNotFoundError::ToolNotFound(name.to_string()))
    }

    /// Look up a tool by name, returning an owned reference.
    pub fn get_tool_arc(&self, name: &str) -> Option<BoxedTool> {
        if let Some(t) = self.baseline.get(name) {
            return Some(Arc::clone(t));
        }
        if let Some(t) = self.gated.get(name) {
            return Some(Arc::clone(t));
        }
        self.dynamic_mcp.read().get(name).map(Arc::clone)
    }

    /// List all registered tools with their names and trait objects.
    #[allow(clippy::significant_drop_tightening)]
    pub fn list_tools(&self) -> Vec<(String, BoxedTool)> {
        let mut tools: Vec<(String, BoxedTool)> = self
            .baseline
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        tools.extend(self.gated.iter().map(|(k, v)| (k.clone(), Arc::clone(v))));
        let mcp = self.dynamic_mcp.read();
        tools.extend(mcp.iter().map(|(k, v)| (k.clone(), Arc::clone(v))));
        tools
    }

    /// Get all tool names available in the given context.
    ///
    /// Applies tier filtering, context allowlists, and deny rules.
    #[allow(clippy::significant_drop_tightening)]
    pub fn available_tool_names(
        &self,
        context: ExecutionContext,
        feature_flags_enabled: bool,
    ) -> Vec<String> {
        let mcp = self.dynamic_mcp.read();
        let mut names: Vec<String> = self
            .baseline
            .keys()
            .cloned()
            .chain(self.gated.keys().cloned())
            .chain(mcp.keys().cloned())
            .collect();

        // Apply allowlist filtering
        if let Some(allowlist) = self.context_allowlists.get(&context) {
            names.retain(|n| allowlist.contains(n));
        }

        // Apply context-specific tier filtering
        names.retain(|name| {
            let is_gated = self.gated.contains_key(name);
            let is_mcp = mcp.contains_key(name);

            match context {
                ExecutionContext::PlanMode => {
                    // Plan mode: only read-only tools
                    if let Some(tool) = mcp.get(name) {
                        return tool.descriptor().is_read_only;
                    }
                    self.get_tool(name)
                        .is_ok_and(|tool| tool.descriptor().is_read_only)
                }
                ExecutionContext::AsyncAgent => {
                    // Async agents can't use UI-dependent tools
                    if let Some(tool) = mcp.get(name) {
                        let desc = tool.descriptor();
                        return desc.is_enabled && !desc.requires_approval;
                    }
                    self.get_tool(name).is_ok_and(|tool| {
                        let desc = tool.descriptor();
                        desc.is_enabled && !desc.requires_approval
                    })
                }
                ExecutionContext::CoordinatorMode => {
                    // Coordinator can only dispatch to other agents/tools
                    self.get_tool(name).is_ok() || mcp.contains_key(name)
                }
                _ => {
                    // Feature-gated tools need the feature flag
                    if is_gated && !feature_flags_enabled {
                        return false;
                    }
                    // MCP tools only in contexts that support them
                    if is_mcp {
                        matches!(
                            context,
                            ExecutionContext::Normal
                                | ExecutionContext::ReplOnly
                                | ExecutionContext::InProcessTeam
                        )
                    } else {
                        true
                    }
                }
            }
        });

        // Apply deny rules
        let is_plan_mode = matches!(context, ExecutionContext::PlanMode);
        names.retain(|name| {
            !self
                .deny_rules
                .iter()
                .any(|rule| rule.matches(name, &[], is_plan_mode))
        });

        names.sort();
        names
    }

    /// Get all registered tools as descriptors.
    #[must_use]
    pub fn all_descriptors(&self) -> Vec<ToolDescriptor> {
        let mcp = self.dynamic_mcp.read();
        self.baseline
            .values()
            .map(|t| t.descriptor())
            .chain(self.gated.values().map(|t| t.descriptor()))
            .chain(mcp.values().map(|t| t.descriptor()))
            .collect()
    }

    /// Check if a tool name is denied in the given context.
    ///
    /// Name-only: this answers "is this tool denied at all", which is what
    /// listing tools needs. A rule that only names paths cannot be judged
    /// without a call to judge it against — see [`Self::denial_for`].
    #[must_use]
    pub fn is_denied(&self, tool_name: &str, context: ExecutionContext) -> bool {
        let is_plan_mode = matches!(context, ExecutionContext::PlanMode);
        self.deny_rules
            .iter()
            .any(|rule| rule.matches(tool_name, &[], is_plan_mode))
    }

    /// Why a call is denied, if a rule denies it.
    ///
    /// This is the check that has to happen per call and with the call's own
    /// arguments: a rule written against a path is only meaningful against the
    /// path the tool is about to touch, and consulting it with an empty list
    /// is the same as not having it.
    #[must_use]
    pub fn denial_for(
        &self,
        tool_name: &str,
        paths: &[PathBuf],
        context: ExecutionContext,
    ) -> Option<String> {
        let is_plan_mode = matches!(context, ExecutionContext::PlanMode);
        self.deny_rules
            .iter()
            .find(|rule| rule.matches(tool_name, paths, is_plan_mode))
            .map(|rule| {
                if rule
                    .tool_names
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(tool_name))
                {
                    format!("tool '{tool_name}' is denied by a rule")
                } else {
                    format!(
                        "tool '{tool_name}' is denied for {}",
                        paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            })
    }

    /// Interrupt behavior for a named tool.
    #[must_use]
    pub fn interrupt_behavior(&self, tool_name: &str) -> InterruptBehavior {
        if let Some(tool) = self.dynamic_mcp.read().get(tool_name) {
            return tool.descriptor().interrupt_behavior;
        }
        self.get_tool(tool_name)
            .map(|t| t.descriptor().interrupt_behavior)
            .unwrap_or(InterruptBehavior::RunToCompletion)
    }

    /// What a named tool's rule exemptions are for this call.
    ///
    /// The flags are a property of the call, not only of the tool — the
    /// artifact tool refuses a whole-tool allow rule for its mutating actions
    /// and honours it for its reads — so the tool is asked with the call's own
    /// arguments. An unknown tool declares nothing.
    #[must_use]
    pub fn rule_exemptions_for(&self, tool_name: &str, input: &Value) -> ToolRuleExemptions {
        self.get_tool_arc(tool_name)
            .map_or(ToolRuleExemptions::NONE, |tool| {
                tool.rule_exemptions_for(input)
            })
    }
}

/// Install a registry-backed rule-exemption resolver on a permission engine.
///
/// The checker lives in `coderex-core` and cannot see tools, and the flags are
/// a property of the *call* rather than the tool, so copying a tool's constant
/// answer into a table is not enough: the engine has to be able to ask, per
/// call, what this input makes of the tool. Without this call a tool's declared
/// `ignores_whole_tool_allow_rule` / `suppresses_always_allow_rule` are inert.
pub fn install_rule_exemptions(
    engine: &mut coderex_core::permission_engine::PermissionEngine,
    registry: &Arc<ToolRegistry>,
) {
    let registry = Arc::clone(registry);
    engine.set_rule_exemptions_resolver(Box::new(move |tool_name, input| {
        registry.rule_exemptions_for(tool_name, input)
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{EchoTool, NoOpTool};
    use crate::context::{ToolError, ToolOutput};
    use crate::tool::{BoxedTool, InterruptBehavior, ToolContext, ToolInput};
    use async_trait::async_trait;
    use coderex_core::permission_engine::PermissionEngine;
    use coderex_core::permissions::{
        PermissionAction, PermissionChecker, PermissionMode, PermissionResult, PermissionRule,
    };
    use std::collections::HashMap;

    fn make_tool(tool: impl Tool + 'static) -> BoxedTool {
        Arc::new(tool)
    }

    /// A tool whose answer depends on the call, the way the artifact tool's
    /// does: it refuses a whole-tool allow rule for its writes and honours it
    /// for its reads.
    ///
    /// It is named `Write` because that is a name the checker already treats as
    /// dangerous, so the ordinary default a refused rule falls through to is
    /// observable from outside.
    #[derive(Debug)]
    struct SplitExemptionsTool;

    #[async_trait]
    impl Tool for SplitExemptionsTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "Write".into(),
                tier: ToolTier::Baseline,
                input_schema: serde_json::json!({ "type": "object" }),
                output_schema: serde_json::json!({ "type": "object" }),
                requires_approval: false,
                is_enabled: true,
                is_read_only: false,
                is_concurrency_safe: false,
                interrupt_behavior: InterruptBehavior::RunToCompletion,
            }
        }

        fn rule_exemptions_for(&self, input: &Value) -> ToolRuleExemptions {
            if input.get("action").and_then(Value::as_str) == Some("publish") {
                ToolRuleExemptions {
                    ignores_whole_tool_allow_rule: true,
                    ..ToolRuleExemptions::NONE
                }
            } else {
                ToolRuleExemptions::NONE
            }
        }

        async fn execute(
            &self,
            _input: ToolInput,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: String::new(),
                is_display: false,
            })
        }
    }

    fn whole_tool_allow(tool: &str) -> PermissionRule {
        PermissionRule {
            tool_name: tool.into(),
            action: PermissionAction::Allow,
            paths: Vec::new(),
            description: String::new(),
        }
    }

    /// The registry answers for the call it is given, and says nothing about a
    /// tool it does not have.
    #[test]
    fn rule_exemptions_are_answered_per_call() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(SplitExemptionsTool));

        assert!(
            registry
                .rule_exemptions_for("Write", &serde_json::json!({ "action": "publish" }))
                .ignores_whole_tool_allow_rule
        );
        assert_eq!(
            registry.rule_exemptions_for("Write", &serde_json::json!({ "action": "read" })),
            ToolRuleExemptions::NONE
        );
        assert_eq!(
            registry.rule_exemptions_for("Write", &serde_json::json!({})),
            ToolRuleExemptions::NONE
        );
        assert_eq!(
            registry.rule_exemptions_for("unknown", &serde_json::json!({ "action": "publish" })),
            ToolRuleExemptions::NONE
        );
    }

    /// A tool's declaration is inert until a session introduces the registry to
    /// the engine. This is that introduction, and the split survives the trip:
    /// the read is approved by the whole-tool allow rule, the publish is not.
    #[tokio::test]
    async fn installed_exemptions_reach_the_engine_per_call() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(SplitExemptionsTool));
        registry.register(make_tool(EchoTool));
        let registry = Arc::new(registry);

        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.add_rule(whole_tool_allow("Write")).unwrap();
        let mut engine = PermissionEngine::new(checker);
        install_rule_exemptions(&mut engine, &registry);

        // The read carries no exemption, so the whole-tool allow rule stands.
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
            "the whole-tool allow rule must stand for a read: {result:?}"
        );

        // The publish refuses the rule, so the decision falls through to the
        // ordinary dangerous default.
        let (_request, result) = engine
            .evaluate(
                "Write",
                serde_json::json!({ "action": "publish" }).to_string(),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        assert!(
            matches!(result, PermissionResult::Ask { .. }),
            "a refused allow rule must not approve the call: {result:?}"
        );

        // A tool that declares nothing is untouched by the resolver.
        let (_request, result) = engine
            .evaluate("Echo", "{}".into(), None, HashMap::new())
            .await
            .unwrap();
        assert!(matches!(result, PermissionResult::Allow));
    }

    /// Deny is evaluated before any allow rule and is not something a tool's
    /// exemptions can lift: the exempt publish is still denied.
    #[tokio::test]
    async fn a_deny_rule_beats_the_exemption() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(SplitExemptionsTool));
        let registry = Arc::new(registry);

        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker.add_rule(whole_tool_allow("Write")).unwrap();
        checker
            .add_rule(PermissionRule {
                tool_name: "Write".into(),
                action: PermissionAction::Deny,
                paths: Vec::new(),
                description: "no writes here".into(),
            })
            .unwrap();
        let mut engine = PermissionEngine::new(checker);
        install_rule_exemptions(&mut engine, &registry);

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
            PermissionResult::Deny { reason } => assert!(reason.contains("no writes here"), "{reason}"),
            other => panic!("deny must win over the exemption, got {other:?}"),
        }
    }

    #[test]
    fn register_and_get_tool_by_name() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(EchoTool));
        registry.register(make_tool(NoOpTool));

        assert!(registry.get_tool("echo").is_ok());
        assert!(registry.get_tool("noop").is_ok());
        assert!(registry.get_tool("unknown").is_err());
    }

    #[test]
    fn all_descriptors_returns_all_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(EchoTool));
        registry.register(make_tool(NoOpTool));

        let descriptors = registry.all_descriptors();
        assert_eq!(descriptors.len(), 2);

        let names: Vec<_> = descriptors.iter().map(|d| &d.name).collect();
        assert!(names.contains(&&"echo".to_string()));
        assert!(names.contains(&&"noop".to_string()));
    }

    /// A path rule is a glob over the whole path. Looking for the pattern as a
    /// substring — which is what this used to do — means `/etc/**` matches
    /// nothing, so a deny rule written the only way it can be written denies
    /// nothing.
    #[test]
    fn a_path_deny_rule_matches_a_glob() {
        let rule = ToolDenyRule::for_paths(vec![
            "/etc/**".to_string(),
            "**/secrets/**".to_string(),
            "/tmp/exact.txt".to_string(),
        ]);
        let matches = |path: &str| rule.matches("Read", &[PathBuf::from(path)], false);

        assert!(matches("/etc/passwd"));
        assert!(matches("/etc/ssh/ssh_config"));
        assert!(matches("/home/me/secrets/token"));
        assert!(matches("/tmp/exact.txt"));

        assert!(!matches("/tmp/notes.txt"));
        assert!(
            !matches("/home/me/secret-notes.md"),
            "a prefix is not a match"
        );
        assert!(!matches("/etcetera/file"));
    }

    #[test]
    fn a_path_rule_that_is_not_a_glob_matches_nothing() {
        let rule = ToolDenyRule::for_paths(vec!["[unclosed".to_string()]);
        assert!(!rule.matches("Read", &[PathBuf::from("/etc/passwd")], false));
    }

    #[test]
    fn a_denial_says_which_of_the_two_it_was() {
        let mut registry = ToolRegistry::new();
        registry.add_deny_rule(ToolDenyRule::for_tools(vec!["Bash".into()]));
        assert_eq!(
            registry.denial_for("Bash", &[], ExecutionContext::Normal),
            Some("tool 'Bash' is denied by a rule".to_string())
        );
        assert!(
            registry
                .denial_for("Read", &[], ExecutionContext::Normal)
                .is_none()
        );

        let mut paths = ToolRegistry::new();
        paths.add_deny_rule(ToolDenyRule::for_paths(vec!["**/secrets/**".into()]));
        let reason = paths
            .denial_for(
                "Read",
                &[PathBuf::from("/home/me/secrets/token")],
                ExecutionContext::Normal,
            )
            .unwrap();
        assert!(reason.contains("secrets/token"), "{reason}");
        assert!(
            paths
                .denial_for(
                    "Read",
                    &[PathBuf::from("/home/me/notes.md")],
                    ExecutionContext::Normal
                )
                .is_none()
        );
    }

    #[test]
    fn deny_rule_blocks_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(EchoTool));
        registry.register(make_tool(NoOpTool));

        registry.add_deny_rule(ToolDenyRule::for_tools(vec!["echo".into()]));

        assert!(registry.is_denied("echo", ExecutionContext::Normal));
        assert!(!registry.is_denied("noop", ExecutionContext::Normal));

        let names = registry.available_tool_names(ExecutionContext::Normal, true);
        assert!(!names.contains(&"echo".to_string()));
        assert!(names.contains(&"noop".to_string()));
    }

    #[test]
    fn context_allowlist_filters_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(EchoTool));
        registry.register(make_tool(NoOpTool));

        registry.set_context_allowlist(
            ExecutionContext::PlanMode,
            HashSet::from(["noop".to_string()]),
        );

        let names = registry.available_tool_names(ExecutionContext::PlanMode, true);
        // echo is not read-only, noop is read-only
        assert!(names.contains(&"noop".to_string()));
        assert!(!names.contains(&"echo".to_string()));
    }

    #[test]
    fn plan_mode_allows_only_read_only_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(EchoTool));
        registry.register(make_tool(NoOpTool));

        // EchoTool is not read-only, NoOpTool is read-only
        let names = registry.available_tool_names(ExecutionContext::PlanMode, true);
        assert!(names.contains(&"noop".to_string()));
        assert!(!names.contains(&"echo".to_string()));
    }

    #[test]
    fn baseline_tools_always_available() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(NoOpTool));

        // NoOpTool is baseline tier, so it appears regardless of flag
        let names_disabled = registry.available_tool_names(ExecutionContext::Normal, false);
        assert!(names_disabled.contains(&"noop".to_string()));

        let names_enabled = registry.available_tool_names(ExecutionContext::Normal, true);
        assert!(names_enabled.contains(&"noop".to_string()));
    }

    #[test]
    fn interrupt_behavior_returns_correct_value() {
        let mut registry = ToolRegistry::new();
        registry.register(make_tool(NoOpTool));

        assert_eq!(
            registry.interrupt_behavior("noop"),
            InterruptBehavior::RunToCompletion
        );
        assert_eq!(
            registry.interrupt_behavior("unknown"),
            InterruptBehavior::RunToCompletion
        );
    }
}
