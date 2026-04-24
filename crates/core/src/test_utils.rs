//! Mock API client for testing — simulates API responses without network calls.

use crate::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use crate::tool_executor::ToolInfo;
use crate::types::ModelId;
use futures::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// A mock API client that returns pre-configured responses.
#[derive(Debug)]
pub struct MockApiClient {
    /// Queue of responses to return in order.
    responses: Arc<Mutex<VecDeque<Result<ApiResponse, ApiError>>>>,
    /// Queue of streaming events to return in order.
    stream_events: Arc<Mutex<VecDeque<Vec<StreamEvent>>>>,
    /// Log of all calls made (system, messages, model, max_tokens).
    call_log: Arc<Mutex<Vec<(String, Vec<serde_json::Value>, String, u32)>>>,
}

impl Clone for MockApiClient {
    fn clone(&self) -> Self {
        Self {
            responses: Arc::clone(&self.responses),
            stream_events: Arc::clone(&self.stream_events),
            call_log: Arc::clone(&self.call_log),
        }
    }
}

impl Default for MockApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockApiClient {
    /// Create a new empty mock client.
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            stream_events: Arc::new(Mutex::new(VecDeque::new())),
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Enqueue a successful response.
    pub fn enqueue_response(&self, content: &str, input_tokens: usize, output_tokens: usize) {
        self.responses.lock().unwrap().push_back(Ok(ApiResponse {
            content: content.into(),
            tool_calls: Vec::new(),
            usage: crate::types::UsageSummary {
                input_tokens,
                output_tokens,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            stop_reason: "end_turn".into(),
        }));
    }

    /// Enqueue an error response.
    pub fn enqueue_error(&self, message: &str) {
        self.responses.lock().unwrap().push_back(Err(ApiError::Auth(message.into())));
    }

    /// Enqueue a stream of events.
    pub fn enqueue_stream(&self, events: Vec<StreamEvent>) {
        self.stream_events.lock().unwrap().push_back(events);
    }

    /// Get the call log.
    #[must_use]
    pub fn call_log(&self) -> Vec<(String, Vec<serde_json::Value>, String, u32)> {
        self.call_log.lock().unwrap().clone()
    }

    /// Get the number of calls made.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.call_log.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl ApiClient for MockApiClient {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(self.clone())
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        self.call_log.lock().unwrap().push((
            system.to_string(),
            messages.to_vec(),
            model.0.clone(),
            max_tokens,
        ));

        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(ApiError::RetryExhausted {
                    attempts: 0,
                    last_error_message: "No more mock responses configured".into(),
                })
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
        let events = self.stream_events.lock().unwrap().pop_front();
        match events {
            Some(evts) => Box::pin(futures::stream::iter(evts)),
            None => Box::pin(futures::stream::once(async {
                StreamEvent::Error(ApiError::RetryExhausted {
                    attempts: 0,
                    last_error_message: "No more mock stream responses configured".into(),
                })
            })),
        }
    }
}
