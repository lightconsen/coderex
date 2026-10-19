//! The memory tool family, adapted to the `Tool` trait.
//!
//! `coderex-memory` cannot implement `coderex_tools::Tool` itself: `coderex-tools`
//! already depends on it (`BriefTool` reads transcripts), so the reverse
//! dependency would close a cycle and Cargo rejects that even for an optional
//! dependency. The family is therefore written against plain JSON — the same
//! shape the trait passes around, with no reference to it — and wrapped here,
//! in a crate that depends on both.
//!
//! The classifier is the other half of this: a shared store is read by other
//! people, so a write to one is decided by the model rather than assumed safe.
//! It fails closed — a query that fails, or an answer that is not an
//! unambiguous yes, refuses the write.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use coderex_core::api_client::ApiClient;
use coderex_memory::service::{MemoryService, MemoryStore, MemoryWriteClassifier};
use coderex_memory::tools::{MemoryTool, MemoryTools};
use coderex_tools::{
    InterruptBehavior, Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolOutput,
    ToolTier,
};

/// System prompt for the shared-memory classifier.
const SHARE_CLASSIFIER_SYSTEM: &str = "\
You decide whether a note may be shared with a team's shared memory store.\n\n\
Answer with ONLY one word:\n\
yes = general project knowledge, conventions, architecture notes, how something works\n\
no = anything personal, secret, credential-shaped, machine-specific, or a one-off\n\
     debugging detail that only made sense in one session";

/// Wraps one memory tool so the registry can hold it.
pub struct MemoryToolAdapter {
    tool: MemoryTool,
    /// Whether writes are allowed.
    ///
    /// `--no-session-persistence` means "nothing writes to disk", and the flag
    /// reaches the session after the tools are registered, so the decision has
    /// to be readable at call time rather than baked in at construction.
    writable: Arc<AtomicBool>,
}

impl MemoryToolAdapter {
    /// Wrap a tool from the family.
    #[must_use]
    pub const fn new(tool: MemoryTool, writable: Arc<AtomicBool>) -> Self {
        Self { tool, writable }
    }
}

impl std::fmt::Debug for MemoryToolAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryToolAdapter")
            .field("tool", &self.tool.name())
            .field("writable", &self.writable.load(Ordering::Relaxed))
            .finish()
    }
}

#[async_trait::async_trait]
impl Tool for MemoryToolAdapter {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.tool.name().to_string(),
            tier: ToolTier::Baseline,
            input_schema: self.tool.input_schema(),
            output_schema: self.tool.output_schema(),
            requires_approval: false,
            is_enabled: true,
            is_read_only: self.tool.is_read_only(),
            is_concurrency_safe: self.tool.is_read_only(),
            interrupt_behavior: InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        if !self.tool.is_read_only() && !self.writable.load(Ordering::Relaxed) {
            return Err(ToolError::InvalidInput {
                message: "memory writes are disabled in this session \
                          (--no-session-persistence)"
                    .to_string(),
            });
        }

        self.tool
            .call(&input.arguments)
            .await
            .map(|output| ToolOutput {
                output,
                is_display: true,
            })
            .map_err(|error| ToolError::InvalidInput {
                message: error.to_string(),
            })
    }
}

/// Every memory tool, ready to register.
#[must_use]
pub fn memory_tools(
    service: Arc<MemoryService>,
    writable: &Arc<AtomicBool>,
) -> Vec<Arc<dyn Tool + Send + Sync>> {
    MemoryTools::new(service)
        .all()
        .into_iter()
        .map(|tool| {
            Arc::new(MemoryToolAdapter::new(tool, Arc::clone(writable)))
                as Arc<dyn Tool + Send + Sync>
        })
        .collect()
}

/// Decides whether a memory may be shared, by asking the lightweight model.
pub struct ModelMemoryClassifier {
    api_client: Box<dyn ApiClient>,
}

impl ModelMemoryClassifier {
    /// Classify shared writes with this client.
    #[must_use]
    pub fn new(api_client: Box<dyn ApiClient>) -> Self {
        Self { api_client }
    }
}

impl std::fmt::Debug for ModelMemoryClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelMemoryClassifier").finish()
    }
}

#[async_trait::async_trait]
impl MemoryWriteClassifier for ModelMemoryClassifier {
    async fn classify(&self, store: MemoryStore, key: &str, content: &str) -> Result<(), String> {
        let prompt = format!(
            "Store: {}\nKey: {key}\nContent:\n{content}\n\nMay this be shared? Answer yes or no.",
            store.as_str()
        );

        let result = coderex_core::orchestrator::run_lightweight_query(
            self.api_client.as_ref(),
            SHARE_CLASSIFIER_SYSTEM,
            &prompt,
            8,
        )
        .await;

        match result {
            Ok(query) => {
                let answer = query.content.trim().to_ascii_lowercase();
                if answer.starts_with("yes") {
                    Ok(())
                } else {
                    // Anything that is not an unambiguous yes is a no: an
                    // unparseable answer must not read as permission.
                    Err(format!(
                        "the shared-memory classifier refused this: {answer}"
                    ))
                }
            }
            // A classifier that cannot be reached cannot grant permission.
            Err(error) => Err(format!("could not classify this shared write: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::api_client::{ApiError, ApiResponse, StreamEvent};
    use coderex_core::tool_executor::ToolInfo;
    use coderex_core::types::{ModelId, UsageSummary};

    /// Answers every side query with a fixed sentence.
    struct FixedApi {
        answer: &'static str,
    }

    #[async_trait::async_trait]
    impl ApiClient for FixedApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                answer: self.answer,
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
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

    /// An API that is down.
    struct FailingApi;

    #[async_trait::async_trait]
    impl ApiClient for FailingApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(FailingApi)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            Err(ApiError::Transport {
                kind: coderex_core::api_client::TransportError::Other,
                class: coderex_core::api_client::ErrorClass::Unknown,
                message: "no route to host".into(),
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

    #[tokio::test]
    async fn a_shared_write_is_allowed_when_the_classifier_says_yes() {
        let classifier = ModelMemoryClassifier::new(Box::new(FixedApi { answer: "yes" }));

        let result = classifier
            .classify(MemoryStore::Shared, "build", "the build uses cargo")
            .await;

        assert!(result.is_ok(), "expected an allow, got {result:?}");
    }

    #[tokio::test]
    async fn a_shared_write_is_refused_when_the_classifier_says_no() {
        let classifier = ModelMemoryClassifier::new(Box::new(FixedApi { answer: "no" }));

        let result = classifier
            .classify(MemoryStore::Shared, "key", "an api key: sk-live-123")
            .await;

        assert!(result.is_err());
    }

    /// An answer that is neither yes nor no must not read as permission.
    #[tokio::test]
    async fn an_ambiguous_answer_is_refused() {
        let classifier = ModelMemoryClassifier::new(Box::new(FixedApi {
            answer: "It depends on the team's policy.",
        }));

        let result = classifier
            .classify(MemoryStore::Shared, "key", "content")
            .await;

        assert!(
            result.is_err(),
            "an ambiguous answer must not allow a write"
        );
    }

    /// A classifier that cannot be reached cannot grant permission.
    #[tokio::test]
    async fn an_unreachable_classifier_refuses_the_write() {
        let classifier = ModelMemoryClassifier::new(Box::new(FailingApi));

        let result = classifier
            .classify(MemoryStore::Shared, "key", "content")
            .await;

        assert!(
            result.is_err(),
            "a failed classification must refuse the write, not allow it"
        );
    }

    fn service_in(dir: &std::path::Path) -> Arc<MemoryService> {
        use coderex_memory::long_term::LongTermStore;
        use coderex_memory::team_memory::TeamMemorySyncService;

        Arc::new(MemoryService::new(
            Arc::new(LongTermStore::new(dir.join("long_term"))),
            Arc::new(TeamMemorySyncService::new(
                "default".to_string(),
                dir.join("team"),
            )),
        ))
    }

    /// `--no-session-persistence` arrives after the tools are registered, so
    /// the flag has to be read at call time rather than captured.
    #[tokio::test]
    async fn a_write_is_refused_when_persistence_is_off() {
        let temp = tempfile::tempdir().unwrap();
        let writable = Arc::new(AtomicBool::new(false));
        let tools = memory_tools(service_in(temp.path()), &writable);

        let write = tools
            .iter()
            .find(|tool| tool.descriptor().name == "memory_write")
            .expect("memory_write is registered");
        let ctx =
            coderex_tools::ToolContext::new(coderex_tools::ExecutionContext::Normal, temp.path());
        let arguments = serde_json::json!({
            "store": "project",
            "key": "k",
            "content": "v",
        });
        let call = |id: &str| ToolInput {
            call_id: id.to_string(),
            name: "memory_write".to_string(),
            arguments: arguments.clone(),
        };

        let refused = write.execute(call("c1"), ctx.clone()).await;
        assert!(
            refused.is_err(),
            "a write went through with persistence off: {refused:?}"
        );

        writable.store(true, Ordering::Relaxed);
        let allowed = write.execute(call("c2"), ctx).await;
        assert!(
            allowed.is_ok(),
            "the same write should succeed once persistence is on: {allowed:?}"
        );
    }
}
