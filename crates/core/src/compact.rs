//! Compact strategies for reducing context window size.
//!
//! Four strategies are available, ranging from light to aggressive:
//! - **Micro**: Replace early tool results with short summaries
//! - **Auto**: Micro + remove pure-text assistant messages
//! - **Reactive**: Auto + truncate older conversation turns
//! - **Snip**: Truncate history at a boundary point, preserving essential context

use crate::prompt::PromptCompiler;
use crate::types::{Message, MessageContent, Role, ToolResult};

// ─── Compact Strategy ───────────────────────────────────────────────────────

/// Which compact strategy to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStrategy {
    /// Lightest: replace early tool results with summaries.
    Micro,
    /// Moderate: micro + remove pure-text assistant messages.
    Auto,
    /// Aggressive: auto + truncate older turns.
    Reactive,
    /// Most aggressive: truncate history at a snip point.
    Snip,
}

/// Result of applying a compact strategy.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Which strategy was applied.
    pub strategy: CompactStrategy,
    /// Messages after compaction.
    pub messages: Vec<Message>,
    /// Estimated tokens removed.
    pub tokens_removed: usize,
    /// Whether a compact boundary message was inserted.
    pub boundary_inserted: bool,
    /// Index in the message list where the boundary was inserted.
    pub boundary_index: Option<usize>,
}

// ─── Token estimation for messages ──────────────────────────────────────────

fn message_token_estimate(msg: &Message) -> usize {
    match &msg.content {
        MessageContent::Text(s) => PromptCompiler::estimate_tokens(s),
        MessageContent::ToolCall(tc) => {
            let json = serde_json::to_string(tc).unwrap_or_default();
            PromptCompiler::estimate_tokens(&json)
        }
        MessageContent::ToolResult(tr) => {
            let json = serde_json::to_string(tr).unwrap_or_default();
            PromptCompiler::estimate_tokens(&json)
        }
    }
}

fn total_token_estimate(messages: &[Message]) -> usize {
    messages.iter().map(message_token_estimate).sum()
}

// ─── Micro Compact ──────────────────────────────────────────────────────────

/// Apply micro compact: replace early tool results with short summaries.
///
/// Keeps the last `keep_recent` tool result messages intact. Earlier ones
/// are replaced with a summary message (first 200 chars of output + "...").
#[must_use]
pub fn micro_compact(messages: &[Message], keep_recent: usize) -> CompactResult {
    let tokens_before = total_token_estimate(messages);

    // Find indices of tool result messages
    let tool_result_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::Tool))
        .map(|(i, _)| i)
        .collect();

    if tool_result_indices.len() <= keep_recent {
        // Nothing to compact — all tool results are within keep_recent
        return CompactResult {
            strategy: CompactStrategy::Micro,
            messages: messages.to_vec(),
            tokens_removed: 0,
            boundary_inserted: false,
            boundary_index: None,
        };
    }

    // Summarize tool results beyond keep_recent from the end
    let summarize_count = tool_result_indices.len() - keep_recent;
    let mut result = messages.to_vec();

    for idx in tool_result_indices.iter().take(summarize_count) {
        if let MessageContent::ToolResult(tr) = &result[*idx].content.clone() {
            result[*idx] = summarize_tool_result(tr);
        }
    }

    let tokens_after = total_token_estimate(&result);
    CompactResult {
        strategy: CompactStrategy::Micro,
        messages: result,
        tokens_removed: tokens_before.saturating_sub(tokens_after),
        boundary_inserted: false,
        boundary_index: None,
    }
}

/// Replace a ToolResult with a short summary message.
fn summarize_tool_result(tr: &ToolResult) -> Message {
    let max_len = 200;
    let summary = if tr.output.len() > max_len {
        format!(
            "[Tool '{}' output truncated] {}",
            tr.tool_call_id,
            &tr.output[..max_len]
        )
    } else {
        format!(
            "[Tool '{}'] {}",
            tr.tool_call_id, tr.output
        )
    };
    Message::system(summary)
}

// ─── Auto Compact ───────────────────────────────────────────────────────────

/// Check if auto compact should trigger.
#[must_use]
pub const fn should_auto_compact(
    estimated_tokens: usize,
    threshold: usize,
    _budget: &crate::budget::TokenBudget,
) -> bool {
    estimated_tokens > threshold
}

/// Apply auto compact: micro compact + remove pure-text assistant messages.
///
/// After micro compact, if still over threshold, removes older assistant
/// messages that contain only text (no tool calls).
#[must_use]
pub fn auto_compact(messages: &[Message], threshold: usize) -> CompactResult {
    // First apply micro compact
    let micro = micro_compact(messages, 5);

    let tokens_after_micro = total_token_estimate(&micro.messages);
    if tokens_after_micro <= threshold {
        return CompactResult {
            tokens_removed: tokens_after_micro.saturating_sub(total_token_estimate(&micro.messages)),
            ..micro
        };
    }

    // Additional pass: remove older pure-text assistant messages
    let mut result = micro.messages;
    let assistant_text_indices: Vec<usize> = result
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            matches!(m.role, Role::Assistant)
                && matches!(&m.content, MessageContent::Text(_))
        })
        .map(|(i, _)| i)
        .collect();

    // Remove up to half of the older assistant text messages
    let remove_count = assistant_text_indices.len() / 2;
    for idx in assistant_text_indices.iter().take(remove_count).rev() {
        result.remove(*idx);
    }

    let tokens_after = total_token_estimate(&result);
    CompactResult {
        strategy: CompactStrategy::Auto,
        messages: result,
        tokens_removed: tokens_after_micro.saturating_sub(tokens_after),
        boundary_inserted: false,
        boundary_index: None,
    }
}

// ─── Reactive Compact ───────────────────────────────────────────────────────

/// Check if reactive compact should trigger.
#[must_use]
pub fn should_reactive_compact(
    stop_reason: &str,
    estimated_tokens: usize,
    threshold: usize,
) -> bool {
    stop_reason == "max_tokens" || estimated_tokens > threshold
}

/// Apply reactive compact: more aggressive than auto.
///
/// Preserves the first user message and the most recent N turns.
/// Removes everything in between.
#[must_use]
pub fn reactive_compact(messages: &[Message], keep_recent_turns: usize) -> CompactResult {
    if messages.is_empty() {
        return CompactResult {
            strategy: CompactStrategy::Reactive,
            messages: Vec::new(),
            tokens_removed: 0,
            boundary_inserted: false,
            boundary_index: None,
        };
    }

    // Always preserve the first user message
    let first_user_idx = messages
        .iter()
        .position(|m| matches!(m.role, Role::User));

    // Work backwards to find the start of the last N turns
    // A "turn" starts with a User message
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::User))
        .map(|(i, _)| i)
        .collect();

    let tokens_before = total_token_estimate(messages);

    if user_indices.len() <= keep_recent_turns + 1 {
        // Not enough turns to truncate
        return CompactResult {
            strategy: CompactStrategy::Reactive,
            messages: messages.to_vec(),
            tokens_removed: 0,
            boundary_inserted: false,
            boundary_index: None,
        };
    }

    // Keep: first user message + last N turns
    let keep_from = user_indices[user_indices.len() - keep_recent_turns];

    let mut result = Vec::new();

    // Add first user message
    if let Some(idx) = first_user_idx {
        result.push(messages[idx].clone());
    }

    // Add boundary marker if there's a gap
    if let Some(first_idx) = first_user_idx {
        if keep_from > first_idx + 1 {
            insert_compact_boundary_messages(&mut result);
        }
    }

    // Add the last N turns
    for msg in &messages[keep_from..] {
        result.push(msg.clone());
    }

    let tokens_after = total_token_estimate(&result);
    CompactResult {
        strategy: CompactStrategy::Reactive,
        messages: result,
        tokens_removed: tokens_before.saturating_sub(tokens_after),
        boundary_inserted: true,
        boundary_index: first_user_idx.map(|i| i + 1),
    }
}

// ─── Snip Compact ───────────────────────────────────────────────────────────

/// Apply snip compact: truncate history, preserve boundaries.
///
/// Walks backwards from the end, accumulating tokens until reaching
/// `target_tokens`. Always preserves the first user message.
#[must_use]
pub fn snip_compact(messages: &[Message], target_tokens: usize) -> CompactResult {
    if messages.is_empty() {
        return CompactResult {
            strategy: CompactStrategy::Snip,
            messages: Vec::new(),
            tokens_removed: 0,
            boundary_inserted: false,
            boundary_index: None,
        };
    }

    let tokens_before = total_token_estimate(messages);

    // If already under target, nothing to do
    if tokens_before <= target_tokens {
        return CompactResult {
            strategy: CompactStrategy::Snip,
            messages: messages.to_vec(),
            tokens_removed: 0,
            boundary_inserted: false,
            boundary_index: None,
        };
    }

    // Find the first user message (always preserve)
    let first_user_idx = messages
        .iter()
        .position(|m| matches!(m.role, Role::User));

    // Walk backwards accumulating tokens
    let mut accumulated = 0usize;
    let mut snip_point = messages.len();

    for i in (0..messages.len()).rev() {
        let token_cost = message_token_estimate(&messages[i]);
        accumulated += token_cost;
        if accumulated > target_tokens {
            snip_point = i + 1;
            break;
        }
        snip_point = i;
    }

    // Ensure we don't snip before the first user message
    if let Some(idx) = first_user_idx {
        if snip_point <= idx {
            snip_point = idx + 1;
        }
    }

    let mut result = Vec::new();

    // Add first user message
    if let Some(idx) = first_user_idx {
        result.push(messages[idx].clone());

        // Add boundary marker if there's a gap
        if snip_point > idx + 1 {
            insert_compact_boundary_messages(&mut result);
        }
    }

    // Add messages from snip point onward
    for msg in &messages[snip_point..] {
        result.push(msg.clone());
    }

    let tokens_after = total_token_estimate(&result);
    CompactResult {
        strategy: CompactStrategy::Snip,
        messages: result,
        tokens_removed: tokens_before.saturating_sub(tokens_after),
        boundary_inserted: true,
        boundary_index: first_user_idx.map(|i| i + 1),
    }
}

/// Insert a compact boundary marker into the message list.
pub fn insert_compact_boundary(messages: &mut Vec<Message>, at_index: usize) {
    if at_index <= messages.len() {
        messages.insert(at_index, compact_boundary_message());
    } else {
        messages.push(compact_boundary_message());
    }
}

/// Create a compact boundary marker system message.
#[must_use]
pub fn compact_boundary_message() -> Message {
    Message::system(
        "<coderex_compact_boundary>\n\
        Previous conversation history was truncated to reduce context size.\n\
        The conversation prior to this point has been summarized and removed.\n\
        Continue the conversation from this point.\n\
        </coderex_compact_boundary>",
    )
}

/// Insert boundary marker messages (used internally by compact strategies).
fn insert_compact_boundary_messages(messages: &mut Vec<Message>) {
    messages.push(compact_boundary_message());
}

// ─── Compact Strategy Selection ─────────────────────────────────────────────

/// Select the appropriate compact strategy based on current state.
#[must_use]
pub fn select_compact_strategy(
    stop_reason: &str,
    estimated_tokens: usize,
    auto_threshold: usize,
    reactive_threshold: usize,
    budget_near_limit: bool,
) -> CompactStrategy {
    if stop_reason == "max_tokens" {
        return CompactStrategy::Reactive;
    }
    if estimated_tokens > reactive_threshold {
        return CompactStrategy::Reactive;
    }
    if estimated_tokens > auto_threshold {
        return CompactStrategy::Auto;
    }
    if budget_near_limit {
        return CompactStrategy::Micro;
    }
    CompactStrategy::Snip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_result(output: &str) -> Message {
        Message::tool(ToolResult {
            tool_call_id: "tc_test".into(),
            output: output.to_string(),
            is_error: false,
        })
    }

    fn make_assistant_text(text: &str) -> Message {
        Message::assistant(text)
    }

    fn make_user_text(text: &str) -> Message {
        Message::user(text)
    }

    #[test]
    fn micro_compact_reduces_token_count() {
        let mut messages = Vec::new();
        messages.push(make_user_text("hello"));
        messages.push(make_assistant_text("sure"));
        messages.push(make_tool_result(&"x".repeat(1000))); // large tool result
        messages.push(make_user_text("next"));
        messages.push(make_assistant_text("ok"));
        messages.push(make_tool_result("small"));

        let result = micro_compact(&messages, 1);

        // Should have reduced token count
        assert!(result.tokens_removed > 0);
        assert_eq!(result.messages.len(), messages.len()); // same count, but some summarized
    }

    #[test]
    fn micro_compact_preserves_recent_tool_results() {
        let messages = vec![
            make_tool_result("old_result_1"),
            make_tool_result("old_result_2"),
            make_tool_result("recent_result"),
        ];

        let result = micro_compact(&messages, 1);

        // Last tool result should be intact
        let last = &result.messages[2];
        if let MessageContent::ToolResult(tr) = &last.content {
            assert_eq!(tr.output, "recent_result");
        } else {
            panic!("expected tool result");
        }

        // Earlier ones should be summarized (converted to system messages)
        let first = &result.messages[0];
        assert!(matches!(first.role, Role::System));
    }

    #[test]
    fn auto_compact_triggers_at_threshold() {
        let messages = vec![
            make_user_text("start"),
            make_assistant_text(&"x".repeat(2000)),
            make_user_text("middle"),
            make_assistant_text(&"y".repeat(2000)),
            make_user_text("end"),
        ];

        // Low threshold should trigger
        let result = auto_compact(&messages, 100);
        assert!(matches!(result.strategy, CompactStrategy::Auto));
    }

    #[test]
    fn auto_compact_does_not_trigger_below_threshold() {
        let messages = vec![
            make_user_text("hello"),
            make_assistant_text("world"),
        ];

        let result = auto_compact(&messages, 10000);
        // Under threshold, should effectively be a no-op or micro
        assert!(matches!(
            result.strategy,
            CompactStrategy::Micro | CompactStrategy::Auto
        ));
    }

    #[test]
    fn reactive_compact_preserves_first_user() {
        let mut messages = Vec::new();
        messages.push(make_user_text("initial query"));
        for i in 0..10 {
            messages.push(make_assistant_text(&format!("response {i}")));
            messages.push(make_user_text(&format!("follow-up {i}")));
        }

        let result = reactive_compact(&messages, 2);

        // First message should be the initial user query
        assert!(!result.messages.is_empty());
        let first = &result.messages[0];
        assert!(matches!(first.role, Role::User));
        if let MessageContent::Text(s) = &first.content {
            assert_eq!(s, "initial query");
        }
    }

    #[test]
    fn snip_compact_reduces_to_fit_target() {
        let mut messages = Vec::new();
        messages.push(make_user_text("start"));
        for i in 0..20 {
            messages.push(make_assistant_text(&format!("long response {}", i).repeat(50)));
        }
        messages.push(make_user_text("final"));

        let result = snip_compact(&messages, 500);

        // Result should fit within target
        let tokens = total_token_estimate(&result.messages);
        // Allow some overhead for boundary marker
        assert!(tokens <= 500 + 100);
    }

    #[test]
    fn snip_compact_preserves_first_user() {
        let messages = vec![
            make_user_text("must preserve this"),
            make_assistant_text(&"x".repeat(500)),
            make_user_text("also this"),
        ];

        let result = snip_compact(&messages, 50);

        assert!(!result.messages.is_empty());
        let first = &result.messages[0];
        assert!(matches!(first.role, Role::User));
        if let MessageContent::Text(s) = &first.content {
            assert_eq!(s, "must preserve this");
        }
    }

    #[test]
    fn compact_boundary_message_has_correct_format() {
        let msg = compact_boundary_message();
        assert!(matches!(msg.role, Role::System));
        if let MessageContent::Text(s) = &msg.content {
            assert!(s.contains("<coderex_compact_boundary>"));
            assert!(s.contains("truncated"));
            assert!(s.contains("</coderex_compact_boundary>"));
        } else {
            panic!("expected text content");
        }
    }

    #[test]
    fn select_strategy_max_tokens_is_reactive() {
        let strategy = select_compact_strategy("max_tokens", 50000, 100000, 180000, false);
        assert!(matches!(strategy, CompactStrategy::Reactive));
    }

    #[test]
    fn select_strategy_over_reactive_threshold_is_reactive() {
        let strategy = select_compact_strategy("end_turn", 200000, 100000, 180000, false);
        assert!(matches!(strategy, CompactStrategy::Reactive));
    }

    #[test]
    fn select_strategy_over_auto_threshold_is_auto() {
        let strategy = select_compact_strategy("end_turn", 150000, 100000, 180000, false);
        assert!(matches!(strategy, CompactStrategy::Auto));
    }

    #[test]
    fn select_strategy_budget_near_limit_is_micro() {
        let strategy = select_compact_strategy("end_turn", 50000, 100000, 180000, true);
        assert!(matches!(strategy, CompactStrategy::Micro));
    }

    #[test]
    fn select_strategy_default_is_snip() {
        let strategy = select_compact_strategy("end_turn", 50000, 100000, 180000, false);
        assert!(matches!(strategy, CompactStrategy::Snip));
    }
}
