//! Tool trait and related types.

use crate::context::{ExecutionContext, ToolError, ToolOutput};
use async_trait::async_trait;
use coderex_core::hook_executor::HookExecutor;
use coderex_core::permissions::ToolRuleExemptions;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Key type for shared state in `ToolContext.extra`.
pub type SharedState = Box<dyn std::any::Any + Send + Sync>;

/// Key in `ToolContext.extra` holding the owning session id as a `String`.
///
/// Tools that write durable job records use it so a record is attributed to
/// the session that started the work. Absent (as in a bare test context), the
/// record is written with no owner, which adoption treats as belonging to
/// whoever is adopting.
pub const JOB_SESSION_ID_KEY: &str = "job_session_id";

/// Key in `ToolContext.extra` holding the session's hook executor as an
/// `Arc<dyn HookExecutor>`.
///
/// The engine fires the hooks it can see (prompt, tool, permission, compact,
/// stop). A few lifecycle moments belong to a tool instead — entering a
/// worktree, completing a task — and the tool fires those through this
/// executor. Absent (as in a bare test context) the tool simply does not
/// fire.
pub const HOOK_EXECUTOR_KEY: &str = "hook_executor";

/// Borrow the session's hook executor out of a tool context, if one is
/// installed.
#[must_use]
pub fn hook_executor_from(ctx: &ToolContext<'_>) -> Option<Arc<dyn HookExecutor>> {
    ctx.extra
        .get(HOOK_EXECUTOR_KEY)
        .and_then(|state| state.downcast_ref::<Arc<dyn HookExecutor>>())
        .cloned()
}

/// The `ToolContext.extra` key under which the session's file checkpoint
/// store rides.
///
/// A mutating edit tool snapshots its target through this store before it
/// writes, so `/rewind` can undo the file half of a truncated turn.
pub const CHECKPOINT_STORE_KEY: &str = "checkpoint_store";

/// Borrow the session's file checkpoint store out of a tool context, if one
/// is installed.
#[must_use]
pub fn checkpoint_store_from(
    ctx: &ToolContext<'_>,
) -> Option<Arc<coderex_core::checkpoints::CheckpointStore>> {
    ctx.extra
        .get(CHECKPOINT_STORE_KEY)
        .and_then(|state| state.downcast_ref::<Arc<coderex_core::checkpoints::CheckpointStore>>())
        .cloned()
}

/// Fire a lifecycle hook through the session's executor, if one is installed.
///
/// A tool that owns a lifecycle moment the engine cannot see (`WorktreeCreate`
/// on entering a worktree, `TaskCompleted` on completing a task) calls this
/// after the moment has happened. With no executor in the context — a bare
/// test context — it is a no-op, so a tool keeps working without hooks.
pub async fn fire_tool_hook(
    ctx: &ToolContext<'_>,
    event: &str,
    input: &str,
    context: &HashMap<String, String>,
) {
    let Some(hooks) = hook_executor_from(ctx) else {
        return;
    };
    let result = hooks.fire_hook(event, input, context).await;
    if !result.errors.is_empty() {
        tracing::warn!(event, errors = ?result.errors, "tool hook errors");
    }
}

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
}

/// A graded per-tool cap on how many characters of output a tool may return.
///
/// A tool declares its own tier instead of the executor guessing, so a tool
/// that legitimately returns a lot and one that never should are
/// distinguishable. Seven tiers, matching the reference's `maxResultSizeChars`
/// ladder.
///
/// [`MaxResultSizeChars::Unlimited`] is the reference's `1`/`0` sentinel: no
/// cap at all. Exactly one tool declares it — `Read` — and only because Read
/// bounds its own output before returning it.
///
/// The ladder tops out at [`MaxResultSizeChars::Huge`]; a tool that needs more
/// than that declares `Unlimited`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaxResultSizeChars {
    /// No cap (the reference's `1`/`0`). Read only.
    Unlimited,
    /// 10 000 characters.
    Tiny,
    /// 20 000 characters.
    Small,
    /// 50 000 characters.
    Medium,
    /// 100 000 characters.
    Large,
    /// 300 000 characters.
    XLarge,
    /// 1 048 576 characters — the widest finite tier, and the flat cap every
    /// tool had before the ladder existed.
    #[default]
    Huge,
}

impl MaxResultSizeChars {
    /// The cap in characters, or `None` for `Unlimited`.
    #[must_use]
    pub const fn chars(self) -> Option<usize> {
        match self {
            Self::Unlimited => None,
            Self::Tiny => Some(10_000),
            Self::Small => Some(20_000),
            Self::Medium => Some(50_000),
            Self::Large => Some(100_000),
            Self::XLarge => Some(300_000),
            Self::Huge => Some(1_048_576),
        }
    }

    /// The tier for a raw character count.
    ///
    /// `0` and `1` are the reference's spellings of "no cap", so they map to
    /// [`MaxResultSizeChars::Unlimited`]; anything else snaps to the nearest
    /// tier at or above it.
    #[must_use]
    pub const fn from_chars(chars: usize) -> Self {
        match chars {
            0 | 1 => Self::Unlimited,
            2..=10_000 => Self::Tiny,
            10_001..=20_000 => Self::Small,
            20_001..=50_000 => Self::Medium,
            50_001..=100_000 => Self::Large,
            100_001..=300_000 => Self::XLarge,
            _ => Self::Huge,
        }
    }

    /// How the reference spells this tier: the character count, or `1` for
    /// `Unlimited`.
    #[must_use]
    pub const fn declared(self) -> usize {
        match self.chars() {
            Some(chars) => chars,
            None => 1,
        }
    }
}

/// The argument a permission rule should read a tool's content from.
///
/// A deny rule written against a path can only be judged against the value the
/// call is about to touch, and only the tool knows which argument carries it:
/// Bash's content is its `command`, Read's is its `file_path`, SendFile's is
/// its `files` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleContentField {
    /// A single string argument (Bash: `command`, Read: `file_path`).
    Scalar(&'static str),
    /// A list of strings, or of objects carrying a `path` (SendFile: `files`).
    List(&'static str),
}

impl RuleContentField {
    /// The JSON argument name.
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::Scalar(field) | Self::List(field) => field,
        }
    }
}

/// How a tool that can outlive its own call behaves.
///
/// Three states: a tool either always completes inside its call (`None`),
/// manages the backgrounding itself and answers with a handle (`SelfManaged`,
/// the reference's `"self"`), or detaches from the turn and reports later
/// (`Detach`, the reference's `"detach"`).
///
/// The state is also a promise about the input schema: a tool that advertises
/// a `run_in_background` argument must declare something other than `None`,
/// and the dispatcher stops advertising the argument for a tool that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backgrounding {
    /// The tool always completes inside its call.
    #[default]
    None,
    /// The tool backgrounds itself and returns a handle the caller can poll.
    SelfManaged,
    /// The tool detaches from the turn; its result arrives later.
    Detach,
}

/// Everything a tool declares about itself that has no home in
/// [`ToolDescriptor`].
///
/// This is a defaulted method on [`Tool`] rather than four more
/// `ToolDescriptor` fields because `ToolDescriptor` is built with a struct
/// literal at well over a hundred sites across the workspace, several of them
/// in crates this one does not own; a new required field would break every one
/// of them. Every default below is the behaviour tools had before the concept
/// existed, so a tool that declares nothing is unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCapabilities {
    /// This tool's output cap.
    pub max_result_size_chars: MaxResultSizeChars,
    /// The argument a permission rule reads this tool's content from.
    ///
    /// `None` means "no declaration", and the caller falls back to looking for
    /// anything path-shaped.
    pub rule_content_field: Option<RuleContentField>,
    /// Whether this tool backgrounds work of its own accord.
    pub backgrounding: Backgrounding,
    /// Whether a whole-tool allow rule (one naming this tool with no path
    /// patterns) must not auto-approve it.
    ///
    /// The rule is not honoured and the decision continues down the ordinary
    /// path; it is not "always ask", and it narrows *allow* rules only — a deny
    /// rule is evaluated first and still wins.
    ///
    /// This is the tool's constant answer. A tool whose answer depends on the
    /// call overrides [`Tool::rule_exemptions_for`] instead.
    pub ignores_whole_tool_allow_rule: bool,
    /// Whether the "always allow" choice must not be offered for this tool, and
    /// an approval must not be written as a session-wide one.
    ///
    /// As above, the constant answer; a per-call answer comes from
    /// [`Tool::rule_exemptions_for`].
    pub suppresses_always_allow_rule: bool,
}

impl ToolCapabilities {
    /// This tool's rule exemptions, as the permission checker spells them.
    #[must_use]
    pub const fn rule_exemptions(self) -> ToolRuleExemptions {
        ToolRuleExemptions {
            ignores_whole_tool_allow_rule: self.ignores_whole_tool_allow_rule,
            suppresses_always_allow_rule: self.suppresses_always_allow_rule,
        }
    }
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

    /// What this tool declares about its own output cap, rule content field,
    /// backgrounding, and rule exemptions.
    ///
    /// See [`ToolCapabilities`]. The default is what every tool did before the
    /// concept existed: the 1 MiB cap, no declared content field, no
    /// backgrounding, no rule exemptions.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::default()
    }

    /// What this tool's rule exemptions are for one specific call.
    ///
    /// [`ToolCapabilities`] answers per tool, but the flags it carries are a
    /// property of the *call*: the artifact tool declares exempt actions
    /// (`publish`, `set`, `reply`, …) and ordinary ones (`read`, `list`,
    /// `verify`), so what it refuses depends on what the call is asking for.
    /// A tool with such a split overrides this; the default is its constant
    /// answer, and a tool that declares nothing keeps returning
    /// [`ToolRuleExemptions::NONE`].
    fn rule_exemptions_for(&self, _input: &Value) -> ToolRuleExemptions {
        self.capabilities().rule_exemptions()
    }

    /// Execute the tool with the given input and execution context.
    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError>;
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
    pub fn new(execution_context: ExecutionContext, cwd: &'a std::path::Path) -> Self {
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
