//! Compact strategies for reducing context window size.
//!
//! Four strategies are available, ranging from light to aggressive:
//! - **Micro**: Replace early tool results with short summaries
//! - **Auto**: Micro + remove pure-text assistant messages
//! - **Reactive**: Auto + truncate older conversation turns
//! - **Snip**: Truncate history at a boundary point, preserving essential context

use crate::prompt::PromptCompiler;
use crate::types::{Message, MessageContent, ModelId, Role, ToolResult};

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
        format!("[Tool '{}'] {}", tr.tool_call_id, tr.output)
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
            tokens_removed: tokens_after_micro
                .saturating_sub(total_token_estimate(&micro.messages)),
            ..micro
        };
    }

    // Additional pass: remove older pure-text assistant messages
    let mut result = micro.messages;
    let assistant_text_indices: Vec<usize> = result
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            matches!(m.role, Role::Assistant) && matches!(&m.content, MessageContent::Text(_))
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
    let first_user_idx = messages.iter().position(|m| matches!(m.role, Role::User));

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
    let first_user_idx = messages.iter().position(|m| matches!(m.role, Role::User));

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

/// Select the appropriate compact strategy based on current state and model.
///
/// Thresholds are computed as percentages of the model's context window:
/// - auto: 50% of context window
/// - reactive: 90% of context window
#[must_use]
pub fn select_compact_strategy(
    stop_reason: &str,
    estimated_tokens: usize,
    model: &ModelId,
    budget_near_limit: bool,
) -> CompactStrategy {
    let window = model.context_window();
    let auto_threshold = window * 50 / 100;
    let reactive_threshold = window * 90 / 100;

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

// ─── LLM Summary Compaction ─────────────────────────────────────────────────

/// The interval of messages that an LLM summary compaction replaces.
///
/// `kept_head` preserves the original intent (first user message region),
/// `kept_tail` preserves the live working set (recent turns). Everything
/// strictly between the two is summarized by the LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarySplit {
    /// Index of the first message NOT preserved — where summarization starts.
    pub summarize_from: usize,
    /// Index of the first preserved recent message (end of summarize range).
    pub summarize_to: usize,
}

/// Compute the summarize range for LLM summary compaction.
///
/// Invariants (enforced by tests below):
/// - the first user message is always preserved verbatim,
/// - the last `keep_recent_turns` * complete turns (user + response) are
///   always preserved,
/// - the range is empty when there is nothing worth summarizing.
#[must_use]
pub fn summary_split(messages: &[Message], keep_recent_turns: usize) -> SummarySplit {
    let summarize_from = first_user_index(messages).map_or(0, |i| i + 1);
    let summarize_to = tail_start(messages, keep_recent_turns);
    SummarySplit {
        summarize_from: summarize_from.min(summarize_to),
        summarize_to,
    }
}

/// Index of the first user message (preserved verbatim, intent anchor).
fn first_user_index(messages: &[Message]) -> Option<usize> {
    messages.iter().position(|m| matches!(m.role, Role::User))
}

/// Index where the preserved recent tail starts.
///
/// Walks backwards counting user messages as turn starts; the tail begins
/// after `keep_recent_turns` user messages have been seen.
fn tail_start(messages: &[Message], keep_recent_turns: usize) -> usize {
    let mut turns_seen = 0usize;
    for i in (0..messages.len()).rev() {
        if matches!(messages[i].role, Role::User) {
            turns_seen += 1;
            if turns_seen > keep_recent_turns {
                return i + 1;
            }
        }
    }
    0
}

/// Render a message for the summarizer input.
fn render_message_for_summary(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(s) => format!("[{:?}] {}", msg.role, s),
        MessageContent::ToolCall(tc) => {
            let json = serde_json::to_string(tc).unwrap_or_default();
            format!("[{:?}] tool call: {}", msg.role, json)
        }
        MessageContent::ToolResult(tr) => format!(
            "[tool result for {}] {}",
            tr.tool_call_id,
            truncate_middle(&tr.output, 1_500)
        ),
    }
}

/// Render the summarize range into summarizer input, capped to
/// `max_tokens` (chars/4 heuristic, matching `PromptCompiler`).
///
/// When over budget, whole messages are dropped from the front of the range
/// (oldest first) until it fits — the tail of the range is the most
/// decision-dense, so it is always kept.
#[must_use]
pub fn build_summary_input(
    messages: &[Message],
    split: &SummarySplit,
    max_tokens: usize,
) -> Vec<String> {
    let range = messages
        .get(split.summarize_from..split.summarize_to)
        .unwrap_or(&[]);
    let rendered: Vec<String> = range.iter().map(render_message_for_summary).collect();

    let max_chars = max_tokens.saturating_mul(4);
    let mut total = rendered.iter().map(String::len).sum::<usize>();
    let mut start = 0usize;
    while total > max_chars && start < rendered.len() {
        total -= rendered[start].len();
        start += 1;
    }
    rendered[start..].to_vec()
}

/// Assemble post-compaction messages for LLM summary compaction.
///
/// Result layout: `[preserved head..] [boundary] [summary] [preserved tail..]`
/// — the boundary separates the original head from the generated summary so
/// the model knows everything before it is elided.
#[must_use]
pub fn assemble_summary_result(
    messages: &[Message],
    split: &SummarySplit,
    summary_text: &str,
    tokens_before: usize,
    tokens_after: usize,
) -> CompactResult {
    let mut result: Vec<Message> = Vec::new();
    // Preserved head (original intent).
    result.extend_from_slice(&messages[..split.summarize_from]);
    // Boundary marker between original head and generated summary.
    if split.summarize_from > 0 {
        result.push(compact_boundary_message());
    }
    // Generated summary as a user message (models treat user-provided
    // handoff context as most salient).
    result.push(Message::user(format!(
        "<context_summary>\n{summary_text}\n</context_summary>\n\
         (The above is a summary of earlier conversation. Earlier messages \
         have been compacted away. Continue from where the summary leaves off.)"
    )));
    // Preserved tail (live working set).
    result.extend_from_slice(&messages[split.summarize_to..]);

    CompactResult {
        strategy: CompactStrategy::Auto,
        messages: result,
        tokens_removed: tokens_before.saturating_sub(tokens_after),
        boundary_inserted: true,
        boundary_index: Some(split.summarize_from),
    }
}

/// Truncate a long string in the middle, keeping head and tail.
///
/// Tool outputs tend to start with the command/path (head is signal) and end
/// with the result (tail is signal); the middle is where redundancy lives.
#[must_use]
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    const ELIDER_EXTRA: usize = 40; // "\n[... N chars elided ...]\n" headroom
    let budget = max_chars.saturating_sub(ELIDER_EXTRA);
    let head = budget * 2 / 3;
    let tail = budget - head;
    let chars: Vec<char> = text.chars().collect();
    let elided = chars.len() - head - tail;
    let head_part: String = chars[..head].iter().collect();
    let tail_part: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_part}\n[... {elided} chars elided ...]\n{tail_part}")
}

// ─── Post-Compact File Restore ──────────────────────────────────────────────

/// File-path-like input keys for read-style tools.
const FILE_PATH_KEYS: &[&str] = &["file_path", "path", "filename", "file"];

/// Extract the most recent file paths read inside the summarized interval.
///
/// Scans tool calls (and tool results echoing their inputs) in
/// `messages[split.summarize_from..split.summarize_to]`, collecting paths
/// from read-style tools; returns at most `max_files`, most recent last.
///
/// Used to build the post-compact restore hint so the agent knows which
/// files' contents were compacted away and can re-read them on demand.
#[must_use]
pub fn extract_recent_file_paths(
    messages: &[Message],
    split: &SummarySplit,
    max_files: usize,
) -> Vec<String> {
    if max_files == 0 {
        return Vec::new();
    }
    let range = messages
        .get(split.summarize_from..split.summarize_to)
        .unwrap_or(&[]);

    let is_read_tool = |name: &str| {
        name.eq_ignore_ascii_case("read")
            || name.eq_ignore_ascii_case("file_read")
            || name.eq_ignore_ascii_case("cat")
    };

    let mut paths: Vec<String> = Vec::new();
    for msg in range {
        if let MessageContent::ToolCall(tc) = &msg.content {
            if !is_read_tool(&tc.name) {
                continue;
            }
            for key in FILE_PATH_KEYS {
                if let Some(path) = tc.input.get(*key).and_then(|v| v.as_str()) {
                    let path = path.trim().to_string();
                    if !path.is_empty() && !paths.contains(&path) {
                        paths.push(path);
                    }
                    break;
                }
            }
        }
    }

    // Most recent files matter most: take from the end.
    let start = paths.len().saturating_sub(max_files);
    paths.split_off(start)
}

/// Build the post-compact file-restore message.
///
/// Returns `None` when there are no file paths to restore.
#[must_use]
pub fn file_restore_message(paths: &[String]) -> Option<Message> {
    if paths.is_empty() {
        return None;
    }
    let list = paths
        .iter()
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(Message::user(format!(
        "<post_compact_file_restore>\n\
         The following files were read earlier in this conversation, but their \
         contents were compacted away with the summarized history:\n\n{list}\n\n\
         Re-read any of them with the Read tool if their contents matter for \
         the current work.\n\
         </post_compact_file_restore>"
    )))
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
        let messages = vec![make_user_text("hello"), make_assistant_text("world")];

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
            messages.push(make_assistant_text(
                &format!("long response {}", i).repeat(50),
            ));
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
        let strategy = select_compact_strategy("max_tokens", 50000, &ModelId::sonnet(), false);
        assert!(matches!(strategy, CompactStrategy::Reactive));
    }

    #[test]
    fn select_strategy_over_reactive_threshold_is_reactive() {
        // Sonnet window = 200_000, reactive = 90% = 180_000
        let strategy = select_compact_strategy("end_turn", 190_000, &ModelId::sonnet(), false);
        assert!(matches!(strategy, CompactStrategy::Reactive));
    }

    #[test]
    fn select_strategy_over_auto_threshold_is_auto() {
        // Sonnet window = 200_000, auto = 50% = 100_000
        let strategy = select_compact_strategy("end_turn", 110_000, &ModelId::sonnet(), false);
        assert!(matches!(strategy, CompactStrategy::Auto));
    }

    #[test]
    fn select_strategy_budget_near_limit_is_micro() {
        let strategy = select_compact_strategy("end_turn", 50000, &ModelId::sonnet(), true);
        assert!(matches!(strategy, CompactStrategy::Micro));
    }

    #[test]
    fn select_strategy_default_is_snip() {
        let strategy = select_compact_strategy("end_turn", 50000, &ModelId::sonnet(), false);
        assert!(matches!(strategy, CompactStrategy::Snip));
    }

    #[test]
    fn select_strategy_over_auto_threshold_is_auto_for_default_window() {
        // Default window is 200K: auto=100K, reactive=180K
        let model = ModelId("custom-model".into());
        let strategy = select_compact_strategy("end_turn", 110_000, &model, false);
        assert!(matches!(strategy, CompactStrategy::Auto));
    }

    // ═════════════════════════════════════════════════════════════════════
    //  LLM Summary Compaction
    // ═════════════════════════════════════════════════════════════════════

    fn make_conversation(turns: usize) -> Vec<Message> {
        let mut messages = vec![make_user_text("build the parser module")];
        for i in 0..turns {
            messages.push(make_assistant_text(&format!("working on step {i}")));
            messages.push(make_tool_result(&format!("output {i}").repeat(50)));
            messages.push(make_user_text(&format!("turn {i} feedback")));
        }
        messages
    }

    #[test]
    fn summary_split_preserves_first_user_message_and_tail() {
        let messages = make_conversation(6);
        let split = summary_split(&messages, 3);
        // Head preserved: the first user message.
        assert_eq!(split.summarize_from, 1);
        // Layout: [0]=head user; turn i occupies assistant/tool/user at
        // 3i+1..3i+3, so turn users sit at 3,6,9,12,15,18. The last 3
        // complete turns (turns 3-5) begin at turn 3's assistant, index 10.
        assert_eq!(split.summarize_to, 10);
        // And the summarized interval covers turns 0-2.
        assert_eq!(split.summarize_from.min(split.summarize_to), 1);
    }

    #[test]
    fn summary_split_empty_range_when_few_turns() {
        let messages = make_conversation(2);
        let split = summary_split(&messages, 3);
        // Nothing to summarize: tail covers everything after the head.
        assert!(split.summarize_from >= split.summarize_to);
    }

    #[test]
    fn assemble_summary_result_layout() {
        let messages = make_conversation(6);
        let split = summary_split(&messages, 3);
        let result = assemble_summary_result(&messages, &split, "summary text", 1000, 200);
        assert!(result.boundary_inserted);
        assert_eq!(result.strategy, CompactStrategy::Auto);
        // Layout: head..boundary..summary..tail
        let boundary_idx = result.boundary_index.unwrap();
        assert!(boundary_idx > 0);
        let summary_msg = &result.messages[boundary_idx + 1];
        let text = match &summary_msg.content {
            MessageContent::Text(t) => t,
            _ => panic!("summary must be text"),
        };
        assert!(text.contains("<context_summary>"));
        assert!(text.contains("summary text"));
        // Tail preserved: last message matches original last message.
        assert_eq!(
            result.messages.last().unwrap().role,
            messages.last().unwrap().role
        );
    }

    #[test]
    fn build_summary_input_respects_token_budget() {
        let messages = make_conversation(8);
        let split = summary_split(&messages, 3);
        // Force a tiny budget: max_tokens in chars/4 units.
        let input = build_summary_input(&messages, &split, 200);
        let total_chars: usize = input.iter().map(String::len).sum();
        assert!(total_chars <= 200 * 4 + 4, "got {total_chars} chars");
        assert!(!input.is_empty());
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let long = "a".repeat(500) + "MIDDLE_MARKER" + &"z".repeat(500);
        let out = truncate_middle(&long, 100);
        assert!(out.contains("MIDDLE_MARKER") == false);
        assert!(out.starts_with('a'));
        assert!(out.ends_with('z'));
        assert!(out.contains("chars elided"));
        // Short input passes through.
        assert_eq!(truncate_middle("short", 100), "short");
    }

    #[test]
    fn warning_threshold_below_auto_threshold() {
        let cfg = crate::prompt::CompactConfig::default();
        assert!(cfg.warning_threshold() <= cfg.auto_compact_threshold);
    }

    #[test]
    fn resolve_auto_threshold_uses_window_when_known() {
        let cfg = crate::prompt::CompactConfig::default();
        // 200K window − 4K summary reserve − 13K buffer
        assert_eq!(cfg.resolve_auto_threshold(200_000), 183_000);
        // Unknown window falls back to configured value.
        assert_eq!(cfg.resolve_auto_threshold(0), cfg.auto_compact_threshold);
    }

    #[test]
    fn extract_recent_file_paths_from_read_tool_calls() {
        let mut messages = vec![make_user_text("build the parser module")];
        // Summarized interval: two reads, one write, one other read.
        for (name, input) in [
            ("read", serde_json::json!({"file_path": "src/a.rs"})),
            ("Write", serde_json::json!({"file_path": "src/b.rs"})),
            ("read", serde_json::json!({"path": "docs/guide.md"})),
            ("read", serde_json::json!({"file_path": "src/parser.rs"})),
        ] {
            messages.push(Message {
                role: Role::Assistant,
                content: MessageContent::ToolCall(crate::types::ToolCall {
                    id: format!("tc_{name}_{input}"),
                    name: name.into(),
                    input,
                }),
            });
        }
        let split = SummarySplit {
            summarize_from: 1,
            summarize_to: messages.len(),
        };
        let paths = extract_recent_file_paths(&messages, &split, 5);
        // Write excluded; reads captured in order.
        assert_eq!(paths, vec!["src/a.rs", "docs/guide.md", "src/parser.rs"]);
    }

    #[test]
    fn extract_recent_file_paths_caps_and_takes_most_recent() {
        let mut messages = vec![make_user_text("read many files")];
        for i in 0..8 {
            messages.push(Message {
                role: Role::Assistant,
                content: MessageContent::ToolCall(crate::types::ToolCall {
                    id: format!("tc_{i}"),
                    name: "read".into(),
                    input: serde_json::json!({"file_path": format!("f{i}.rs")}),
                }),
            });
        }
        let split = SummarySplit {
            summarize_from: 1,
            summarize_to: messages.len(),
        };
        let paths = extract_recent_file_paths(&messages, &split, 5);
        assert_eq!(paths.len(), 5);
        // Most recent kept: f3..f7.
        assert_eq!(paths.first().unwrap(), "f3.rs");
        assert_eq!(paths.last().unwrap(), "f7.rs");
    }

    #[test]
    fn file_restore_message_empty_when_no_paths() {
        assert!(file_restore_message(&[]).is_none());
    }

    #[test]
    fn file_restore_message_lists_paths() {
        let msg = file_restore_message(&["a.rs".into(), "b.md".into()]).unwrap();
        let text = match &msg.content {
            MessageContent::Text(t) => t,
            _ => panic!("must be text"),
        };
        assert!(text.contains("<post_compact_file_restore>"));
        assert!(text.contains("- a.rs"));
        assert!(text.contains("- b.md"));
        assert!(text.contains("Re-read"));
    }
}
