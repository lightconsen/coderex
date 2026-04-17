//! Tool registry — manages baseline, feature-gated, and MCP tools with
//! context-based filtering and deny rules.

use crate::context::ExecutionContext;
use crate::tool::{BoxedTool, InterruptBehavior, Tool, ToolDescriptor, ToolTier};
use parking_lot::RwLock;
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
    fn matches(&self, tool_name: &str, path_args: &[PathBuf], plan_mode: bool) -> bool {
        if plan_mode && !self.applies_in_plan_mode {
            return false;
        }
        if !self.tool_names.is_empty() {
            return self.tool_names.iter().any(|n| n == tool_name);
        }
        // Path-based rules match if any path arg matches a glob
        if !self.path_globs.is_empty() {
            for path in path_args {
                for glob in &self.path_globs {
                    if path.to_string_lossy().contains(glob) {
                        return true;
                    }
                }
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
            ToolTier::Baseline | ToolTier::Internal => {
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
        tools.extend(
            self.gated
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v))),
        );
        let mcp = self.dynamic_mcp.read();
        tools.extend(
            mcp.iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v))),
        );
        tools
    }

    /// Get all tool names available in the given context.
    ///
    /// Applies tier filtering, context allowlists, and deny rules.
    #[allow(clippy::significant_drop_tightening)]
    pub fn available_tool_names(&self, context: ExecutionContext, feature_flags_enabled: bool) -> Vec<String> {
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
    #[must_use]
    pub fn is_denied(&self, tool_name: &str, context: ExecutionContext) -> bool {
        let is_plan_mode = matches!(context, ExecutionContext::PlanMode);
        self.deny_rules
            .iter()
            .any(|rule| rule.matches(tool_name, &[], is_plan_mode))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{EchoTool, NoOpTool};
    use crate::tool::{BoxedTool, InterruptBehavior};

    fn make_tool(tool: impl Tool + 'static) -> BoxedTool {
        Arc::new(tool)
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
