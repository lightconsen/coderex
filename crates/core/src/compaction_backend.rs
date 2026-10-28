//! Compaction backend abstraction — the local/remote dual-track seam.
//!
//! Modeled on codex's compaction split: a *local* backend summarizes the
//! conversation with a side LLM query (the current built-in path), while a
//! *remote* backend delegates compaction to a provider-side endpoint (with
//! server-side retention control, e.g. image budgets). The engine consults
//! an injected backend when one is attached and otherwise falls back to the
//! built-in local path, so this trait carries no behavior change on its own.
//!
//! No remote implementation exists yet (deliberately deferred); the trait
//! defines the contract so remote backends can be added without touching the
//! compaction pipeline again.

use async_trait::async_trait;

/// Where a compaction backend executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionKind {
    /// Summarizes with a side LLM query from the client (the built-in path).
    Local,
    /// Delegates compaction to a provider/server-side endpoint.
    Remote,
}

/// A compaction (summarization) request.
///
/// The conversation text is already rendered and capped to the summary-input
/// budget by the caller (`compact::build_summary_input`).
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    /// Rendered conversation interval to summarize.
    pub conversation: String,
    /// Optional focus instruction (e.g. "focus on the auth changes").
    pub focus: Option<String>,
    /// Output budget for the summary.
    pub max_output_tokens: u32,
}

/// Backend that performs the summarization step of compaction.
///
/// Implementations are where a future remote/server-side compaction would
/// plug in; the engine treats an error (or an empty summary) as a signal to
/// fall back to the built-in local path and then rule-based strategies.
#[async_trait]
pub trait CompactionBackend: Send + Sync {
    /// Where this backend executes.
    fn kind(&self) -> CompactionKind;

    /// Short backend name for logs/events (e.g. "local-llm", "responses-v2").
    fn name(&self) -> &str;

    /// Produce a summary for the requested conversation interval.
    async fn summarize(&self, request: CompactionRequest) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBackend;

    #[async_trait]
    impl CompactionBackend for StubBackend {
        fn kind(&self) -> CompactionKind {
            CompactionKind::Remote
        }

        fn name(&self) -> &'static str {
            "stub-remote"
        }

        async fn summarize(&self, request: CompactionRequest) -> Result<String, String> {
            if request.max_output_tokens == 0 {
                Err("no budget".into())
            } else {
                Ok(format!("summary of {} chars", request.conversation.len()))
            }
        }
    }

    #[tokio::test]
    async fn stub_backend_roundtrip_via_trait_object() {
        let backend: std::sync::Arc<dyn CompactionBackend> = std::sync::Arc::new(StubBackend);
        assert_eq!(backend.kind(), CompactionKind::Remote);
        assert_eq!(backend.name(), "stub-remote");

        let out = backend
            .summarize(CompactionRequest {
                conversation: "hello world".into(),
                focus: None,
                max_output_tokens: 4096,
            })
            .await
            .unwrap();
        assert!(out.contains("11 chars"));

        // Error path surfaces through the same contract.
        assert!(
            backend
                .summarize(CompactionRequest {
                    conversation: "x".into(),
                    focus: None,
                    max_output_tokens: 0,
                })
                .await
                .is_err()
        );
    }
}
