//! AI-powered memory workflows.
//!
//! This module is the public entry point for memory AI workflows.
//! The concrete implementations live in `session_memory_agent` and
//! `extract_memories`.

pub use crate::extract_memories::{
    extract_long_term_memories_ai, extract_single_memory_ai,
};
pub use crate::session_memory_agent::update_session_memory_ai;

/// Errors from AI memory workflows.
#[derive(Debug, thiserror::Error)]
pub enum MemoryWorkflowError {
    /// Orchestrator-level error.
    #[error("AI error: {0}")]
    Ai(#[from] coderex_core::orchestrator::OrchestratorError),
    /// Storage I/O error.
    #[error("storage error: {0}")]
    Storage(#[from] std::io::Error),
    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
pub(crate) mod tests {
    use coderex_core::api_client::{ApiClient, ApiResponse, StreamEvent};
    use coderex_core::types::{ModelId, UsageSummary};
    use std::pin::Pin;
    use futures::Stream;

    #[derive(Clone)]
    pub struct MockApi { pub response_content: String }

    impl MockApi {
        pub fn new(content: &str) -> Self { Self { response_content: content.into() } }
    }

    #[async_trait::async_trait]
    impl ApiClient for MockApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
        ) -> Result<ApiResponse, coderex_core::api_client::ApiError> {
            Ok(ApiResponse {
                content: self.response_content.clone(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 100,
                    output_tokens: 50,
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
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
    }
}
