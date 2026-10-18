//! Tool executor — dispatches tool calls with concurrency control and timeout.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolCapabilities, ToolContext, ToolDescriptor, ToolInput};
use crate::tool_cache::{ToolCache, ToolCacheConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

/// Configuration for the tool executor.
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    /// Default timeout for tool execution.
    pub default_timeout: Duration,
    /// Maximum number of concurrent tool executions.
    pub max_concurrent: usize,
    /// Whether to enforce sandbox restrictions for tool execution.
    pub sandbox_enabled: bool,
    /// Operator ceiling on output size, in characters.
    ///
    /// A tool's own tier ([`ToolCapabilities::max_result_size_chars`]) decides
    /// its cap; this only ever lowers it, so a tool that declares less is not
    /// silently given more. `Read` declares "no cap", and no cap means no cap —
    /// it is not lowered by this ceiling either.
    pub max_result_size_chars: usize,
    /// Cache configuration for tool results.
    pub cache_config: ToolCacheConfig,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(60),
            max_concurrent: 4,
            sandbox_enabled: true,
            // The widest finite tier, so a tool that declares nothing keeps
            // exactly the flat cap it had before the ladder existed.
            max_result_size_chars: 1_048_576,
            cache_config: ToolCacheConfig::default(),
        }
    }
}

/// Executes tools with concurrency control, timeout, and result caching.
#[derive(Debug)]
pub struct ToolExecutor {
    config: ExecutionConfig,
    /// Per-tool-name locks for non-concurrency-safe tools.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Optional global result cache for read-only tools.
    cache: Option<ToolCache>,
}

impl ToolExecutor {
    /// Create a new executor with the given config.
    #[must_use]
    pub fn new(config: ExecutionConfig) -> Self {
        let cache = if config.cache_config.enabled {
            Some(ToolCache::new(config.cache_config.clone()))
        } else {
            None
        };
        Self {
            config,
            locks: Mutex::new(HashMap::new()),
            cache,
        }
    }

    /// Return whether sandboxing is enabled.
    #[must_use]
    pub const fn sandbox_enabled(&self) -> bool {
        self.config.sandbox_enabled
    }

    /// Get or create a lock for a specific tool name.
    async fn get_lock(&self, tool_name: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(tool_name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// The lock a call must hold while it runs, if any.
    ///
    /// Concurrency-safe tools take none. Everything else is serialized per
    /// tool name: two calls to the same non-safe tool never overlap, while
    /// two different non-safe tools still may.
    async fn call_lock(&self, desc: &ToolDescriptor) -> Option<Arc<Mutex<()>>> {
        if desc.is_concurrency_safe {
            None
        } else {
            Some(self.get_lock(&desc.name).await)
        }
    }

    /// Execute a single tool call.
    ///
    /// Respects the tool's concurrency safety flag:
    /// - Concurrency-safe tools may run in parallel
    /// - Non-safe tools are serialized per-tool-name
    ///
    /// Read-only results are cached and served from cache when possible.
    pub async fn execute_single(
        &self,
        tool: &dyn Tool,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let desc = tool.descriptor();
        let lock = self.call_lock(&desc).await;
        let _guard = match &lock {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };

        run_tool_call(&desc, tool, &input, ctx, &self.config, self.cache.as_ref()).await
    }
}

/// The number of characters this tool may return, if any.
///
/// The tool's declared tier is the cap; the operator ceiling can only lower
/// it, never raise it. `None` means no cap at all.
fn output_cap(config: &ExecutionConfig, caps: ToolCapabilities) -> Option<usize> {
    caps.max_result_size_chars
        .chars()
        .map(|tier| tier.min(config.max_result_size_chars))
}

/// Truncate tool output to at most `cap` characters.
///
/// Counted in characters, not bytes. `String::truncate` takes a byte index and
/// panics when the cut lands inside a multi-byte character, which a byte-sized
/// cap hits as soon as the output is not ASCII; cutting on a character
/// boundary cannot. It also means the cap means the same thing for CJK output
/// as for English.
fn truncate_output(mut output: ToolOutput, cap: Option<usize>) -> ToolOutput {
    let Some(cap) = cap else {
        return output;
    };
    if let Some((byte_index, _)) = output.output.char_indices().nth(cap) {
        output.output.truncate(byte_index);
        output.output.push_str("\n... [truncated]");
    }
    output
}

/// Run one tool call to completion: timeout, truncation, result caching.
///
/// The caller is responsible for whatever serialization the descriptor calls
/// for; this is the part every entry point shares.
async fn run_tool_call(
    desc: &ToolDescriptor,
    tool: &dyn Tool,
    input: &ToolInput,
    ctx: ToolContext<'_>,
    config: &ExecutionConfig,
    cache: Option<&ToolCache>,
) -> Result<ToolOutput, ToolError> {
    // The cap is the tool's own declaration, read once: the executor no longer
    // guesses a size for every tool alike.
    let cap = output_cap(config, tool.capabilities());

    // Try cache for read-only tools
    if desc.is_read_only {
        if let Some(cache) = cache {
            if let Some(cached) = cache.get(&desc.name, &input.arguments).await {
                return Ok(truncate_output(cached, cap));
            }
        }
    }

    let result = timeout(config.default_timeout, tool.execute(input.clone(), ctx))
        .await
        .map_err(|_| ToolError::Timeout {
            timeout: config.default_timeout,
        })?;

    let result = result.map(|output| truncate_output(output, cap));

    // Cache successful read-only results
    if desc.is_read_only {
        if let (Some(cache), Ok(output)) = (cache, &result) {
            cache
                .insert(&desc.name, &input.arguments, output.clone())
                .await;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{EchoTool, NoOpTool};
    use crate::tool::ToolContext;
    use crate::tool::ToolInput;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records how many copies of itself were inside `execute` at once.
    struct OverlapProbe {
        name: &'static str,
        concurrency_safe: bool,
        inside: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        hold: Duration,
    }

    #[async_trait::async_trait]
    impl Tool for OverlapProbe {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.name.to_string(),
                tier: crate::tool::ToolTier::Baseline,
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                requires_approval: false,
                is_enabled: true,
                is_read_only: false,
                is_concurrency_safe: self.concurrency_safe,
                interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
            }
        }

        async fn execute(
            &self,
            _input: ToolInput,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.hold).await;
            self.inside.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput {
                output: self.name.to_string(),
                is_display: false,
            })
        }
    }

    fn probe(
        name: &'static str,
        concurrency_safe: bool,
        inside: &Arc<AtomicUsize>,
        peak: &Arc<AtomicUsize>,
    ) -> Arc<dyn Tool> {
        Arc::new(OverlapProbe {
            name,
            concurrency_safe,
            inside: Arc::clone(inside),
            peak: Arc::clone(peak),
            hold: Duration::from_millis(20),
        })
    }

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(crate::context::ExecutionContext::Normal, Path::new("/tmp"))
    }

    fn test_input(msg: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "echo".into(),
            arguments: serde_json::json!({"message": msg}),
        }
    }

    #[tokio::test]
    async fn execute_single_echo_tool() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let result = executor
            .execute_single(&EchoTool, test_input("hello"), test_ctx())
            .await
            .unwrap();
        assert_eq!(result.output, "hello");
    }

    #[tokio::test]
    async fn execute_single_noop_tool() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let result = executor
            .execute_single(&NoOpTool, test_input(""), test_ctx())
            .await
            .unwrap();
        assert!(result.output.is_empty());
    }

    #[tokio::test]
    async fn execute_single_serialises_non_concurrency_safe_tools() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let first = probe("edit", false, &inside, &peak);
        let second = probe("edit", false, &inside, &peak);
        let (a, b) = tokio::join!(
            executor.execute_single(first.as_ref(), test_input("a"), test_ctx()),
            executor.execute_single(second.as_ref(), test_input("b"), test_ctx()),
        );

        assert!(a.is_ok() && b.is_ok());
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "execute_single let two calls to the same non-safe tool overlap"
        );
    }

    /// Returns whatever it is told to, so a cap can be observed on the way out.
    struct CappedTool {
        caps: ToolCapabilities,
        output: String,
    }

    #[async_trait::async_trait]
    impl Tool for CappedTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "capped".into(),
                tier: crate::tool::ToolTier::Baseline,
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                requires_approval: false,
                is_enabled: true,
                is_read_only: false,
                is_concurrency_safe: true,
                interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
            }
        }

        fn capabilities(&self) -> ToolCapabilities {
            self.caps
        }

        async fn execute(
            &self,
            _input: ToolInput,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: self.output.clone(),
                is_display: false,
            })
        }
    }

    fn capped(caps: ToolCapabilities, output: String) -> CappedTool {
        CappedTool { caps, output }
    }

    fn medium_caps() -> ToolCapabilities {
        ToolCapabilities {
            max_result_size_chars: crate::tool::MaxResultSizeChars::Medium,
            ..ToolCapabilities::default()
        }
    }

    /// A tool's own tier is the cap — the executor no longer applies one size
    /// to every tool alike.
    #[tokio::test]
    async fn a_tool_that_declares_a_smaller_tier_is_cut_to_it() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let tool = capped(medium_caps(), "x".repeat(60_000));

        let out = executor
            .execute_single(&tool, test_input(""), test_ctx())
            .await
            .unwrap();

        assert_eq!(
            out.output.chars().count(),
            50_000 + "\n... [truncated]".len()
        );
        assert!(out.output.ends_with("\n... [truncated]"));
    }

    /// The default tier is the old flat cap, so the 45 tools that declare
    /// nothing are unchanged.
    #[tokio::test]
    async fn a_tool_that_declares_nothing_keeps_the_old_flat_cap() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let tool = capped(ToolCapabilities::default(), "x".repeat(1_048_577));

        let out = executor
            .execute_single(&tool, test_input(""), test_ctx())
            .await
            .unwrap();

        assert_eq!(
            out.output.chars().count(),
            1_048_576 + "\n... [truncated]".len()
        );
    }

    /// `1`/`0` in the reference means "no cap", and no cap means untouched.
    #[tokio::test]
    async fn an_unlimited_tool_is_not_truncated() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let tool = capped(
            ToolCapabilities {
                max_result_size_chars: crate::tool::MaxResultSizeChars::Unlimited,
                ..ToolCapabilities::default()
            },
            "x".repeat(2_000_000),
        );

        let out = executor
            .execute_single(&tool, test_input(""), test_ctx())
            .await
            .unwrap();

        assert_eq!(out.output.chars().count(), 2_000_000);
        assert!(!out.output.contains("truncated"));
    }

    /// The cap counts characters. A byte-index cut inside a multi-byte
    /// character panics; this output is three bytes per character, so a byte
    /// cap would land mid-character and a naive `String::truncate` would blow
    /// up here.
    #[tokio::test]
    async fn multibyte_output_is_cut_on_a_character_boundary() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let tool = capped(
            ToolCapabilities {
                max_result_size_chars: crate::tool::MaxResultSizeChars::Tiny,
                ..ToolCapabilities::default()
            },
            "漢".repeat(10_001),
        );

        let out = executor
            .execute_single(&tool, test_input(""), test_ctx())
            .await
            .unwrap();

        assert_eq!(
            out.output.chars().count(),
            10_000 + "\n... [truncated]".len()
        );
        assert!(out.output.starts_with("漢"));
    }

    /// The operator ceiling can lower a tool's declared tier, never raise it.
    #[tokio::test]
    async fn the_operator_ceiling_lowers_a_declared_tier() {
        let executor = ToolExecutor::new(ExecutionConfig {
            max_result_size_chars: 100,
            ..ExecutionConfig::default()
        });
        let tool = capped(medium_caps(), "x".repeat(1_000));

        let out = executor
            .execute_single(&tool, test_input(""), test_ctx())
            .await
            .unwrap();

        assert_eq!(out.output.chars().count(), 100 + "\n... [truncated]".len());
    }
}
