//! Tool trait and related types.

use crate::context::{ExecutionContext, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Key type for shared state in `ToolContext.extra`.
pub type SharedState = Box<dyn std::any::Any + Send + Sync>;

/// How a tool should behave when interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptBehavior {
    /// Tool can be interrupted at any time.
    Interruptible,
    /// Tool runs to completion once started.
    RunToCompletion,
}

/// The tier a tool belongs to, controlling its availability.
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

/// Input to a tool execution call.
#[derive(Debug, Clone)]
pub struct ToolInput {
    /// Tool call ID for correlation.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// JSON arguments for the tool.
    pub arguments: Value,
}

/// Metadata and capabilities for a registered tool.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    /// Unique tool name.
    pub name: String,
    /// Which tier this tool belongs to.
    pub tier: ToolTier,
    /// JSON schema for input validation.
    pub input_schema: Value,
    /// JSON schema for output format.
    pub output_schema: Value,
    /// Whether this tool requires user approval.
    pub requires_approval: bool,
    /// Whether this tool is currently enabled.
    pub is_enabled: bool,
    /// Whether this tool only reads data (no side effects).
    pub is_read_only: bool,
    /// Whether this tool can run concurrently with other instances.
    pub is_concurrency_safe: bool,
    /// How this tool behaves when interrupted.
    pub interrupt_behavior: InterruptBehavior,
}

/// Trait for all tools in the system.
///
/// Tools are identified by name and accept JSON arguments. They return
/// a `ToolOutput` on success or a `ToolError` on failure.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Returns the tool's descriptor with metadata and capabilities.
    fn descriptor(&self) -> ToolDescriptor;

    /// Execute the tool with the given input and execution context.
    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError>;
}

/// Request to elicit answers from the user mid-execution.
#[derive(Debug)]
pub struct ElicitRequest {
    /// Questions to present to the user.
    pub questions: Vec<serde_json::Value>,
    /// Channel to receive answers back.
    pub response_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// Runtime context provided to tool execution.
#[derive(Clone)]
pub struct ToolContext<'a> {
    /// The current execution context mode.
    pub execution_context: ExecutionContext,
    /// Working directory for the tool.
    pub cwd: &'a std::path::Path,
    /// Additional shared state (e.g., task stores, registries).
    pub extra: Arc<HashMap<String, SharedState>>,
    /// Optional channel for asking the user questions mid-execution.
    pub elicit_tx: Option<tokio::sync::mpsc::Sender<ElicitRequest>>,
}

impl fmt::Debug for ToolContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("execution_context", &self.execution_context)
            .field("cwd", &self.cwd)
            .field("has_elicit_tx", &self.elicit_tx.is_some())
            .finish()
    }
}

impl<'a> ToolContext<'a> {
    /// Create a new tool context.
    #[must_use]
    pub fn new(
        execution_context: ExecutionContext,
        cwd: &'a std::path::Path,
    ) -> Self {
        Self {
            execution_context,
            cwd,
            extra: Arc::new(HashMap::new()),
            elicit_tx: None,
        }
    }

    /// Add extra context values.
    #[must_use]
    pub fn with_extra(mut self, extra: HashMap<String, SharedState>) -> Self {
        self.extra = Arc::new(extra);
        self
    }

    /// Attach an elicitation channel.
    #[must_use]
    pub fn with_elicit_tx(mut self, tx: tokio::sync::mpsc::Sender<ElicitRequest>) -> Self {
        self.elicit_tx = Some(tx);
        self
    }
}

/// Convenience for storing tools as trait objects.
pub type BoxedTool = Arc<dyn Tool>;
