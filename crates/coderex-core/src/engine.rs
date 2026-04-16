//! QueryEngine — the core conversation and tool execution engine.

/// Placeholder for the QueryEngine implementation.
///
/// The QueryEngine handles:
/// - Building context from session state
/// - Sending requests to the API
/// - Streaming responses
/// - Parsing and executing tool calls
/// - Managing token budget and cost tracking
/// - Running hooks at key lifecycle points
/// - Compact/recovery logic
#[derive(Debug)]
pub struct QueryEngine {}

impl QueryEngine {
    /// Create a new QueryEngine instance.
    pub const fn new() -> Self {
        Self {}
    }
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new()
    }
}
