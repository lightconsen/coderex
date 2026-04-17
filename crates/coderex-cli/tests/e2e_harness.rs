//! E2E test harness — mock server and test utilities.

use std::collections::{HashMap, VecDeque};

/// Mock server state that simulates API responses.
#[derive(Debug)]
pub struct MockServer {
    responses: VecDeque<MockResponse>,
    call_history: Vec<CallRecord>,
}

#[derive(Debug)]
struct CallRecord {
    prompt: String,
    timestamp: std::time::Instant,
}

#[derive(Debug)]
pub struct MockResponse {
    pub content: String,
    pub stop_reason: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
}

impl Default for MockServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MockServer {
    /// Create a new mock server.
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: VecDeque::new(),
            call_history: Vec::new(),
        }
    }

    /// Enqueue a canned response.
    pub fn enqueue(&mut self, content: &str) {
        self.responses.push_back(MockResponse {
            content: content.into(),
            stop_reason: "end_turn".into(),
            input_tokens: 10,
            output_tokens: content.split_whitespace().count(),
        });
    }

    /// Enqueue an error response.
    pub fn enqueue_error(&mut self, message: &str) {
        self.responses.push_back(MockResponse {
            content: format!("[error] {message}"),
            stop_reason: "error".into(),
            input_tokens: 5,
            output_tokens: 0,
        });
    }

    /// Get the next response (simulating API call).
    pub fn respond(&mut self, prompt: &str) -> MockResponse {
        self.call_history.push(CallRecord {
            prompt: prompt.into(),
            timestamp: std::time::Instant::now(),
        });
        self.responses.pop_front().unwrap_or_else(|| MockResponse {
            content: String::new(),
            stop_reason: "stop".into(),
            input_tokens: 0,
            output_tokens: 0,
        })
    }

    /// Get call count.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.call_history.len()
    }

    /// Get all prompts sent to the server.
    #[must_use]
    pub fn prompts(&self) -> Vec<&str> {
        self.call_history.iter().map(|c| c.prompt.as_str()).collect()
    }
}

/// Test context for running e2e scenarios.
pub struct E2EContext {
    /// The mock server.
    pub server: MockServer,
    /// Feature flags.
    pub feature_flags: HashMap<String, bool>,
    /// Temp directory for test files.
    pub temp_dir: tempfile::TempDir,
}

impl E2EContext {
    /// Create a new test context.
    #[must_use]
    pub fn new() -> Self {
        Self {
            server: MockServer::new(),
            feature_flags: HashMap::new(),
            temp_dir: tempfile::tempdir().unwrap(),
        }
    }

    /// Enable a feature flag.
    pub fn enable_feature(&mut self, name: &str) {
        self.feature_flags.insert(name.into(), true);
    }

    /// Get the temp directory path.
    #[must_use]
    pub fn temp_path(&self) -> &std::path::Path {
        self.temp_dir.path()
    }
}

impl Default for E2EContext {
    fn default() -> Self {
        Self::new()
    }
}
