//! Mock API client for testing.
//!
//! Provides [`MockApiClient`] — a test-double for [`ApiClient`] that
//! returns pre-programmed responses instead of hitting a real API.

use coderex_core::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use coderex_core::tool_executor::ToolInfo;
use coderex_core::types::{ModelId, UsageSummary};
use futures::Stream;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// A mock API client that returns pre-programmed responses.
///
/// Use [`mock_response`] and [`mock_streaming_response`] to build
/// the responses it should return.
#[derive(Debug, Clone)]
pub struct MockApiClient {
    responses: Arc<Mutex<Vec<MockResponse>>>,
    call_count: Arc<Mutex<usize>>,
}

#[derive(Debug, Clone)]
enum MockResponse {
    Static(ApiResponse),
    Streaming(Vec<StreamEvent>),
    Error(ApiError),
}

impl MockApiClient {
    /// Create a new mock client with the given responses.
    ///
    /// Responses are returned in order for each call to
    /// `create_message` or `create_message_streaming`.
    #[must_use]
    pub fn new(responses: Vec<ApiResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(
                responses.into_iter().map(MockResponse::Static).collect(),
            )),
            call_count: Arc::new(Mutex::new(0)),
        }
    }

    /// Create a mock client from a mix of response types.
    #[must_use]
    pub fn with_mixed(responses: Vec<MockResponseType>) -> Self {
        let mapped: Vec<MockResponse> = responses
            .into_iter()
            .map(|r| match r {
                MockResponseType::Static(r) => MockResponse::Static(r),
                MockResponseType::Streaming(events) => MockResponse::Streaming(events),
                MockResponseType::Error(e) => MockResponse::Error(e),
            })
            .collect();
        Self {
            responses: Arc::new(Mutex::new(mapped)),
            call_count: Arc::new(Mutex::new(0)),
        }
    }

    fn next_response(&self) -> Option<MockResponse> {
        let mut count = self.call_count.lock().unwrap();
        let responses = self.responses.lock().unwrap();
        let idx = *count;
        *count += 1;
        drop(count);
        responses.get(idx).cloned()
    }

    /// Get the number of calls made to this mock.
    #[must_use]
    pub fn call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

/// A response type for [`MockApiClient::with_mixed`].
#[derive(Debug, Clone)]
pub enum MockResponseType {
    /// Non-streaming response.
    Static(ApiResponse),
    /// Streaming response events.
    Streaming(Vec<StreamEvent>),
    /// Error response.
    Error(ApiError),
}

#[async_trait::async_trait]
impl ApiClient for MockApiClient {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(self.clone())
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        match self.next_response() {
            Some(MockResponse::Static(r)) => Ok(r),
            Some(MockResponse::Error(e)) => Err(e),
            _ => Ok(ApiResponse {
                content: "mock fallback".into(),
                tool_calls: Vec::new(),
                thinking: None,
                usage: UsageSummary {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
            }),
        }
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let events = match self.next_response() {
            Some(MockResponse::Streaming(e)) => e,
            Some(MockResponse::Static(r)) => vec![
                StreamEvent::TextDelta(r.content),
                StreamEvent::UsageSummary(r.usage),
                StreamEvent::StreamEnd,
            ],
            Some(MockResponse::Error(e)) => vec![StreamEvent::Error(e)],
            _ => vec![StreamEvent::StreamEnd],
        };

        Box::pin(futures::stream::iter(events))
    }
}

/// Build a static API response for testing.
#[must_use]
pub fn mock_response(content: impl Into<String>) -> ApiResponse {
    ApiResponse {
        content: content.into(),
        tool_calls: Vec::new(),
        thinking: None,
        usage: UsageSummary {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        stop_reason: "end_turn".into(),
    }
}

/// Build a static API response with tool calls for testing.
#[must_use]
pub fn mock_response_with_tool_calls(
    content: impl Into<String>,
    tool_calls: Vec<coderex_core::types::ToolCall>,
) -> ApiResponse {
    ApiResponse {
        content: content.into(),
        tool_calls,
        thinking: None,
        usage: UsageSummary {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        stop_reason: "end_turn".into(),
    }
}

/// Build a streaming response with text deltas for testing.
#[must_use]
pub fn mock_streaming_response(chunks: Vec<&str>) -> Vec<StreamEvent> {
    let mut events: Vec<StreamEvent> = chunks
        .into_iter()
        .map(|s| StreamEvent::TextDelta(s.into()))
        .collect();
    events.push(StreamEvent::UsageSummary(UsageSummary {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    }));
    events.push(StreamEvent::StreamEnd);
    events
}
