//! TestRig — spins up a QueryEngine with mock dependencies.
//!
//! [`TestRig`] provides a batteries-included harness for testing
//! engine behavior: mock API client, stub tool executor, temp
//! directory, and an event collector.

use coderex_core::api_client::ApiClient;
use coderex_core::engine::{QueryEngine, SubmitOptions, TerminalReason};
use coderex_core::permission_engine::PermissionEngine;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::state::SessionState;
use coderex_core::tool_executor::{ToolDispatchOutcome, ToolExecutor};
use coderex_core::types::{EngineEvent, ModelId, ToolResult};
use serde_json::Value;
use tokio::sync::mpsc;

/// A pre-configured QueryEngine + event collector for testing.
///
/// # Example
///
/// ```rust,no_run
/// use coderex_test_utils::TestRig;
///
/// # async fn example() {
/// let mut rig = TestRig::new().await;
/// let reason = rig.submit("Hello").await.unwrap();
/// let events = rig.collect_events().await;
/// # }
/// ```
pub struct TestRig {
    /// The engine under test.
    pub engine: QueryEngine,
    /// Event receiver.
    pub event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    /// Temporary working directory.
    pub temp_dir: tempfile::TempDir,
}

impl TestRig {
    /// Create a new TestRig with sensible defaults.
    ///
    /// - Mock API client (returns "hello from mock")
    /// - No-op tool executor
    /// - Bypass permissions
    /// - Temp directory as cwd
    pub async fn new() -> Self {
        TestRigBuilder::new().build().await
    }

    /// Submit a prompt and wait for the turn to complete.
    ///
    /// # Errors
    /// Returns an error if the engine encounters an error.
    pub async fn submit(
        &mut self,
        prompt: impl Into<String>,
    ) -> Result<TerminalReason, String> {
        self.engine
            .submit(prompt.into(), SubmitOptions::default())
            .await
    }

    /// Collect all events currently in the channel.
    ///
    /// Non-blocking — drains whatever is buffered.
    #[must_use]
    pub fn drain_events(&mut self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Collect all events, waiting until the channel closes.
    ///
    /// This will block forever if the engine never emits `SessionEnd`.
    pub async fn collect_events(&mut self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.event_rx.recv().await {
            let is_end = matches!(event, EngineEvent::SessionEnd);
            events.push(event);
            if is_end {
                break;
            }
        }
        events
    }

    /// Wait for a specific event type.
    ///
    /// Returns `None` if the channel closes before the event is seen.
    pub async fn wait_for_event<F>(&mut self, predicate: F) -> Option<EngineEvent>
    where
        F: Fn(&EngineEvent) -> bool,
    {
        while let Some(event) = self.event_rx.recv().await {
            if predicate(&event) {
                return Some(event);
            }
        }
        None
    }
}

/// Builder for [`TestRig`].
pub struct TestRigBuilder {
    api_client: Option<Box<dyn ApiClient>>,
    tool_executor: Option<Box<dyn ToolExecutor + Send + Sync>>,
    permission_mode: PermissionMode,
    model: ModelId,
}

impl std::fmt::Debug for TestRigBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestRigBuilder")
            .field("permission_mode", &self.permission_mode)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl TestRigBuilder {
    /// Create a new builder with defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            api_client: None,
            tool_executor: None,
            permission_mode: PermissionMode::BypassPermissions,
            model: ModelId::sonnet(),
        }
    }

    /// Set the API client.
    #[must_use]
    pub fn with_api_client(mut self, client: Box<dyn ApiClient>) -> Self {
        self.api_client = Some(client);
        self
    }

    /// Set the tool executor.
    #[must_use]
    pub fn with_tool_executor(mut self, executor: Box<dyn ToolExecutor + Send + Sync>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    /// Set the permission mode.
    #[must_use]
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    /// Set the model.
    #[must_use]
    pub fn with_model(mut self, model: ModelId) -> Self {
        self.model = model;
        self
    }

    /// Build the TestRig.
    pub async fn build(self) -> TestRig {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cwd = temp_dir.path().to_path_buf();
        let session = SessionState::new(cwd.clone());

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let checker = PermissionChecker::new(self.permission_mode);
        let perm_engine = PermissionEngine::new(checker);

        let api_client = self.api_client.unwrap_or_else(|| {
            Box::new(crate::mock_api::MockApiClient::new(vec![
                crate::mock_api::mock_response("hello from mock"),
            ]))
        });

        let mut engine = QueryEngine::new(api_client, self.model, session, perm_engine, event_tx);

        if let Some(tools) = self.tool_executor {
            engine.set_tool_executor(tools);
        }

        TestRig {
            engine,
            event_rx,
            temp_dir,
        }
    }
}

impl Default for TestRigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A stub tool executor that echoes back successful results.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubOkToolExecutor;

#[async_trait::async_trait]
impl ToolExecutor for StubOkToolExecutor {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        _input: &Value,
    ) -> ToolDispatchOutcome {
        ToolDispatchOutcome {
            result: ToolResult {
                tool_call_id: tool_call_id.to_string(),
                output: format!("stub ok: {tool_name}"),
                is_error: false,
            },
        }
    }
}
