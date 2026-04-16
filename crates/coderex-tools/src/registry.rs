//! Tool registry — manages baseline, feature-gated, and MCP tools.
#![allow(clippy::zero_sized_map_values)]

use std::collections::HashMap;
use dashmap::DashMap;

/// The tier a tool belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTier {
    /// Always available baseline tools.
    Baseline,
    /// Gated behind feature flags.
    FeatureGated,
    /// Dynamically discovered via MCP.
    DynamicMcp,
    /// Internal/debugging tools.
    Internal,
}

/// Placeholder for the Tool trait.
///
/// The actual trait will define:
/// - name, tier, input_schema, output_schema
/// - requires_approval, is_enabled, is_read_only
/// - is_concurrency_safe, interrupt_behavior
/// - async execute() method
#[derive(Debug)]
#[allow(dead_code, clippy::zero_sized_map_values)]
pub struct ToolRegistry {
    baseline: HashMap<String, ()>,       // TODO: Arc<dyn Tool>
    gated: HashMap<String, ()>,           // TODO: Arc<dyn Tool>
    dynamic_mcp: DashMap<String, ()>,     // TODO: Arc<dyn Tool>
    context_allowlists: HashMap<ExecutionContext, ()>,  // TODO: HashSet<String>
    deny_rules: Vec<()>,                  // TODO: ToolDenyRule
}

/// Execution context that filters available tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionContext {
    /// Normal execution context.
    Normal,
    /// Async agent execution context.
    AsyncAgent,
    /// Coordinator mode context.
    CoordinatorMode,
    /// In-process team context.
    InProcessTeam,
    /// Plan mode context.
    PlanMode,
    /// REPL-only context.
    ReplOnly,
    /// Headless SDK context.
    HeadlessSdk,
    /// Remote bridge context.
    RemoteBridge,
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
            dynamic_mcp: DashMap::new(),
            context_allowlists: HashMap::new(),
            deny_rules: Vec::new(),
        }
    }
}
