//! Compact strategies for reducing context window size.
//!
//! Every strategy here is **client-side**: it rewrites the message list in
//! this process, before the request is sent. CodeRex has no server-side
//! context management, so nothing in this module corresponds to the reference
//! implementation's `clear_tool_uses_20250919` context-management instruction,
//! where the request tells the *server* to drop old tool results and the local
//! transcript is left untouched. Do not read these functions as a wrapper over
//! that mechanism — they are the whole of CodeRex's compaction, and the model
//! sees whatever this code leaves behind.
//!
//! Four strategies are available, ranging from light to aggressive:
//! - **Micro**: Truncate early tool results to their head and tail
//! - **Auto**: Micro + remove pure-text assistant messages
//! - **Reactive**: Auto + truncate older conversation turns
//! - **Snip**: Truncate history at a boundary point, preserving essential context

use crate::prompt::{CompactConfig, PromptCompiler};
use crate::types::{ContextWindowResolver, Message, MessageContent, ModelId, Role, ToolResult};

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

/// Parameters for micro compact.
///
/// `head_tokens` and `tail_tokens` split the truncation budget between the two
/// ends of a result (see [`truncate_tool_output`]); only the middle is elided.
#[derive(Debug, Clone, Copy)]
pub struct MicroCompactParams {
    /// Keep the last N tool-result messages verbatim.
    pub keep_recent: usize,
    /// Only truncate tool results larger than this (tokens, chars/4 estimate).
    pub min_result_tokens: usize,
    /// Head tokens kept of a truncated tool result (paths/commands live here).
    pub head_tokens: usize,
    /// Tail tokens kept of a truncated tool result (final output lives here).
    pub tail_tokens: usize,
}

impl Default for MicroCompactParams {
    fn default() -> Self {
        Self {
            keep_recent: 5,
            min_result_tokens: 2_000,
            head_tokens: 1_000,
            tail_tokens: 500,
        }
    }
}

/// Tools whose results micro compact may truncate.
///
/// Only re-derivable results are listed: re-running the command or re-reading
/// the file reconstructs them, so eliding the middle loses nothing that cannot
/// be recovered. Mutating tools (`edit`, `write`, `apply_patch`, …) are
/// deliberately absent — their output records a change that cannot be replayed,
/// and for a diff the elided middle is where the remaining hunks live. A tool
/// that is not listed here keeps its output verbatim.
const MICRO_COMPACT_TOOLS: &[&str] = &[
    "read",
    "shell",
    "bash",
    "grep",
    "rg",
    "glob",
    "websearch",
    "web_search",
    "webfetch",
    "web_fetch",
];

fn is_micro_compactable(name: &str) -> bool {
    MICRO_COMPACT_TOOLS.contains(&name.to_ascii_lowercase().as_str())
}

/// Apply micro compact: shrink early whitelisted tool results in place.
///
/// Keeps the last `keep_recent` tool result messages verbatim. Earlier
/// ones belonging to whitelisted tools and larger than
/// `min_result_tokens` have their output truncated (head + tail kept,
/// middle elided). The message identity — role, `tool_call_id`,
/// `is_error` — is preserved so provider-side tool_use/tool_result
/// pairing survives compaction.
///
/// This edits the transcript itself, so it invalidates any prompt cache over
/// the truncated prefix: only the `keep_recent` results the model most recently
/// saw are left byte-for-byte identical, and everything before the first
/// truncated result is a cache miss on the next request.
#[must_use]
pub fn micro_compact(messages: &[Message], params: MicroCompactParams) -> CompactResult {
    let tokens_before = total_token_estimate(messages);

    // Map tool_call_id → tool name from the assistant ToolCall messages.
    let tool_names: std::collections::HashMap<&str, &str> = messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::ToolCall(tc) => Some((tc.id.as_str(), tc.name.as_str())),
            _ => None,
        })
        .collect();

    // Find indices of tool result messages
    let tool_result_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::Tool))
        .map(|(i, _)| i)
        .collect();

    if tool_result_indices.len() <= params.keep_recent {
        // Nothing to compact — all tool results are within keep_recent
        return CompactResult {
            strategy: CompactStrategy::Micro,
            messages: messages.to_vec(),
            tokens_removed: 0,
            boundary_inserted: false,
            boundary_index: None,
        };
    }

    // Shrink tool results beyond keep_recent from the end
    let summarize_count = tool_result_indices.len() - params.keep_recent;
    let mut result = messages.to_vec();

    for idx in tool_result_indices.iter().take(summarize_count) {
        if let MessageContent::ToolResult(tr) = &result[*idx].content {
            // Conservative: leave unpaired, non-whitelisted or small results
            // intact — never rewrite content we don't actually shrink.
            let name = tool_names.get(tr.tool_call_id.as_str()).copied();
            let Some(name) = name else { continue };
            if !is_micro_compactable(name) {
                continue;
            }
            if PromptCompiler::estimate_tokens(&tr.output) <= params.min_result_tokens {
                continue;
            }
            let truncated =
                truncate_tool_output(&tr.output, params.head_tokens, params.tail_tokens);
            result[*idx] = Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    tool_call_id: tr.tool_call_id.clone(),
                    output: truncated,
                    is_error: tr.is_error,
                }),
            };
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

/// Truncate a tool output keeping head and tail with a token-denominated
/// elision marker.
///
/// Tool outputs tend to start with the command/path (head is signal) and end
/// with the result (tail is signal); the middle is where redundancy lives.
/// The 2:1 head:tail split reflects that tool outputs are more
/// path/command-heavy up front than plain prose.
///
/// Both ends are always kept, which is what makes this safe for the results
/// that matter most: a `cargo test` failure ends with the failing assertion,
/// and a diff ends with its last hunk, so neither is reduced to a bare head.
#[must_use]
pub fn truncate_tool_output(output: &str, head_tokens: usize, tail_tokens: usize) -> String {
    if PromptCompiler::estimate_tokens(output) <= head_tokens + tail_tokens {
        return output.to_string();
    }
    let chars: Vec<char> = output.chars().collect();
    // The budgets below are char-denominated (≈ tokens × 4) but the check
    // above counts *bytes*, so multi-byte output can clear the token budget
    // while still holding fewer chars than `head + tail`. Clamp both ends to
    // the string and never let them overlap: an overlap duplicates the text
    // and returns something longer than the input, the opposite of what a
    // compact pass is for. When the budgets cover the whole string there is
    // nothing in the middle to elide, so the output passes through untouched.
    let head_chars = (head_tokens * 4).min(chars.len());
    let tail_chars = (tail_tokens * 4).min(chars.len() - head_chars);
    if head_chars + tail_chars >= chars.len() {
        return output.to_string();
    }
    let elided_tokens = PromptCompiler::estimate_tokens(output) - head_tokens - tail_tokens;
    let head_part: String = chars[..head_chars].iter().collect();
    let tail_start = chars.len() - tail_chars;
    let tail_part: String = chars[tail_start..].iter().collect();
    format!("{head_part}\n[... ~{elided_tokens} tokens elided ...]\n{tail_part}")
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
    let micro = micro_compact(messages, MicroCompactParams::default());

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

/// Where a session's context sits relative to the compaction lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextLevel {
    /// Room to spare.
    Ok,
    /// Approaching the line: worth telling the user, nothing to do yet.
    Warn,
    /// At the line: compact before sending another request.
    Compact,
    /// The window is spent; a request can no longer be sent.
    Blocked,
}

/// The lines a session's context is measured against, in tokens.
///
/// Claude's arithmetic: the window is what the model can hold, some of it is
/// held back for the reply and the system prompt, and compaction has to happen
/// before the two collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactThresholds {
    /// Compaction line: `window − min(output_reserve, 20 000) − 13 000`.
    pub auto: usize,
    /// Warning line, one buffer below the compaction line.
    pub warn: usize,
    /// Blocked line: `window − 3 000`.
    pub blocked: usize,
}

impl CompactThresholds {
    /// Resolve the lines for a model's context window.
    ///
    /// Uses the default override chain: process environment, the client-data
    /// file, the built-in model table, and finally the conservative unknown
    /// floor. Callers that own settings or experiment state should use
    /// [`Self::resolve_with`] so those sources participate too.
    #[must_use]
    pub fn resolve(model: &ModelId, config: &CompactConfig) -> Self {
        Self::resolve_with(
            model,
            config,
            &ContextWindowResolver::from_env_and_client_data(),
        )
    }

    /// Resolve the lines through an explicit context-window override chain.
    #[must_use]
    pub fn resolve_with(
        model: &ModelId,
        config: &CompactConfig,
        resolver: &ContextWindowResolver,
    ) -> Self {
        Self::resolve_for_window(config, resolver.resolve(&model.0))
    }

    /// Resolve the lines for a context window that has already been looked up.
    ///
    /// Useful when a caller has a provider-reported window or a resolved value
    /// in hand and does not want a second lookup.
    #[must_use]
    pub fn resolve_for_window(config: &CompactConfig, window: usize) -> Self {
        Self {
            auto: config.resolve_auto_threshold(window),
            warn: config.resolve_warning_threshold(window),
            blocked: config.resolve_blocked_threshold(window),
        }
    }

    /// Which line `estimated_tokens` has reached.
    #[must_use]
    pub const fn level(&self, estimated_tokens: usize) -> ContextLevel {
        if estimated_tokens >= self.blocked {
            ContextLevel::Blocked
        } else if estimated_tokens >= self.auto {
            ContextLevel::Compact
        } else if estimated_tokens >= self.warn {
            ContextLevel::Warn
        } else {
            ContextLevel::Ok
        }
    }
}

/// Select the appropriate compact strategy based on current state and model.
///
/// The lines come from the model's context window (see [`CompactThresholds`]),
/// not from a percentage of it: a 200K window compacts at 167K, not at 100K, so
/// context is not thrown away while there is still room for it.
#[must_use]
pub fn select_compact_strategy(
    stop_reason: &str,
    estimated_tokens: usize,
    model: &ModelId,
    config: &CompactConfig,
) -> CompactStrategy {
    select_compact_strategy_with(
        stop_reason,
        estimated_tokens,
        model,
        config,
        &ContextWindowResolver::from_env_and_client_data(),
    )
}

/// Select a compact strategy through an explicit context-window override chain.
///
/// This is the settings/experiment-aware form of
/// [`select_compact_strategy`]; the strategy arithmetic is identical.
#[must_use]
pub fn select_compact_strategy_with(
    stop_reason: &str,
    estimated_tokens: usize,
    model: &ModelId,
    config: &CompactConfig,
    resolver: &ContextWindowResolver,
) -> CompactStrategy {
    if stop_reason == "max_tokens" {
        return CompactStrategy::Reactive;
    }

    let thresholds = CompactThresholds::resolve_with(model, config, resolver);
    match thresholds.level(estimated_tokens) {
        // The window is spent: keep only what still fits.
        ContextLevel::Blocked => CompactStrategy::Snip,
        ContextLevel::Compact => CompactStrategy::Auto,
        // Claude warns at this level rather than compacting. When a caller has
        // asked for recovery anyway, the lightest pass is the right floor —
        // never a truncation, which is what the most aggressive strategy does.
        ContextLevel::Warn | ContextLevel::Ok => CompactStrategy::Micro,
    }
}

// ─── Cache TTL trigger ──────────────────────────────────────────────────────

/// How long after the last request the prompt cache counts as cold.
///
/// The reference implementation wakes at 90% of the TTL and exits 30 seconds
/// early; for a 5-minute cache both rules name the same instant. Taking the
/// earlier of the two keeps that true for other TTLs too.
///
/// A TTL shorter than the early-exit window falls back to the 90% rule: an
/// early exit that has already passed would otherwise make every request
/// count as a cold cache.
#[must_use]
pub fn cache_ttl_wake_after(
    ttl: std::time::Duration,
    early_exit: std::time::Duration,
) -> std::time::Duration {
    let ninety_percent = ttl * 9 / 10;
    match ttl.checked_sub(early_exit) {
        Some(early) if !early.is_zero() => {
            if ninety_percent < early {
                ninety_percent
            } else {
                early
            }
        }
        _ => ninety_percent,
    }
}

/// Whether an idle gap has reached the point where the cache is nearly cold.
///
/// This is a *timing* trigger, not a size one: it says "compacting now costs
/// nothing extra", not "the context is too big". It is deliberately
/// independent of [`CompactThresholds`] so the two triggers can be reasoned
/// about separately.
#[must_use]
pub fn should_compact_on_cache_ttl(
    idle: std::time::Duration,
    ttl: std::time::Duration,
    early_exit: std::time::Duration,
) -> bool {
    !ttl.is_zero() && idle >= cache_ttl_wake_after(ttl, early_exit)
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

    /// A paired assistant ToolCall + tool result exchange, as produced by
    /// a real turn. `micro_compact` needs the ToolCall side to resolve the
    /// tool name for the whitelist check.
    fn make_tool_exchange(id: &str, name: &str, output: &str) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::ToolCall(crate::types::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    input: serde_json::json!({}),
                }),
            },
            Message::tool(ToolResult {
                tool_call_id: id.into(),
                output: output.to_string(),
                is_error: false,
            }),
        ]
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
        messages.extend(make_tool_exchange("tc_1", "read", &"x".repeat(40_000))); // large read result
        messages.push(make_user_text("next"));
        messages.push(make_assistant_text("ok"));
        messages.extend(make_tool_exchange("tc_2", "read", "small"));

        let result = micro_compact(
            &messages,
            MicroCompactParams {
                keep_recent: 1,
                ..Default::default()
            },
        );

        // Should have reduced token count
        assert!(result.tokens_removed > 0);
        assert_eq!(result.messages.len(), messages.len()); // same count, output shrunk
    }

    #[test]
    fn micro_compact_preserves_recent_tool_results() {
        let mut messages = Vec::new();
        messages.extend(make_tool_exchange("tc_1", "read", &"a".repeat(40_000)));
        messages.extend(make_tool_exchange("tc_2", "read", &"b".repeat(40_000)));
        messages.extend(make_tool_exchange("tc_3", "read", "recent_result"));

        let result = micro_compact(
            &messages,
            MicroCompactParams {
                keep_recent: 1,
                ..Default::default()
            },
        );

        // Last tool result should be intact
        let last = &result.messages[5];
        if let MessageContent::ToolResult(tr) = &last.content {
            assert_eq!(tr.output, "recent_result");
        } else {
            panic!("expected tool result");
        }

        // Earlier ones keep their message identity (Tool role, same id)
        // and their output is shrunk, not converted to a system message.
        let first = &result.messages[1];
        assert!(matches!(first.role, Role::Tool));
        if let MessageContent::ToolResult(tr) = &first.content {
            assert_eq!(tr.tool_call_id, "tc_1");
            assert!(tr.output.contains("tokens elided"));
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn micro_compact_preserves_tool_result_identity() {
        let mut messages = Vec::new();
        messages.extend(make_tool_exchange("tc_1", "read", &"y".repeat(40_000)));
        messages.extend(make_tool_exchange("tc_2", "read", "recent"));

        let result = micro_compact(
            &messages,
            MicroCompactParams {
                keep_recent: 1,
                ..Default::default()
            },
        );

        // Message count unchanged, role/tool_call_id/is_error unchanged.
        assert_eq!(result.messages.len(), messages.len());
        let first = &result.messages[1];
        assert!(matches!(first.role, Role::Tool));
        if let MessageContent::ToolResult(tr) = &first.content {
            assert_eq!(tr.tool_call_id, "tc_1");
            assert!(!tr.is_error);
            assert!(tr.output.len() < 40_000);
            assert!(tr.output.contains("tokens elided"));
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn micro_compact_respects_tool_whitelist() {
        let mut messages = Vec::new();
        messages.extend(make_tool_exchange("tc_1", "task", &"z".repeat(40_000)));
        messages.extend(make_tool_exchange(
            "tc_2",
            "unknown_tool",
            &"z".repeat(40_000),
        ));
        messages.extend(make_tool_exchange("tc_3", "read", "recent"));

        let result = micro_compact(
            &messages,
            MicroCompactParams {
                keep_recent: 1,
                ..Default::default()
            },
        );

        // Non-whitelisted tools keep their output verbatim.
        assert_eq!(result.tokens_removed, 0);
        if let MessageContent::ToolResult(tr) = &result.messages[1].content {
            assert_eq!(tr.output, "z".repeat(40_000));
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn micro_compact_leaves_mutating_tool_results_verbatim() {
        // A mutating tool's output records a change that cannot be replayed,
        // and a large one is usually a diff whose elided middle is where the
        // remaining hunks live. Unlike a re-derivable read/shell result, it
        // must never be truncated.
        for name in ["edit", "write", "apply_patch"] {
            let mut messages = Vec::new();
            messages.extend(make_tool_exchange("tc_1", name, &"d".repeat(40_000)));
            messages.extend(make_tool_exchange("tc_2", "read", "recent"));

            let result = micro_compact(
                &messages,
                MicroCompactParams {
                    keep_recent: 1,
                    ..Default::default()
                },
            );

            assert_eq!(result.tokens_removed, 0, "{name} result was truncated");
            if let MessageContent::ToolResult(tr) = &result.messages[1].content {
                assert_eq!(tr.output, "d".repeat(40_000), "{name} output changed");
            } else {
                panic!("expected tool result for {name}");
            }
        }
    }

    #[test]
    fn micro_compact_leaves_small_results_intact() {
        let mut messages = Vec::new();
        messages.extend(make_tool_exchange("tc_1", "read", "tiny output"));
        messages.extend(make_tool_exchange("tc_2", "read", "recent"));

        let result = micro_compact(
            &messages,
            MicroCompactParams {
                keep_recent: 1,
                ..Default::default()
            },
        );

        // Small results are not rewritten (no-op preserves cache).
        assert_eq!(result.tokens_removed, 0);
        if let MessageContent::ToolResult(tr) = &result.messages[1].content {
            assert_eq!(tr.output, "tiny output");
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn micro_compact_unpaired_result_untouched() {
        let messages = vec![
            make_tool_result(&"orphan".repeat(2000)),
            make_tool_result("recent"),
        ];

        let result = micro_compact(
            &messages,
            MicroCompactParams {
                keep_recent: 1,
                ..Default::default()
            },
        );

        // No paired ToolCall → conservative no-op.
        assert_eq!(result.tokens_removed, 0);
        assert!(matches!(result.messages[0].role, Role::Tool));
    }

    #[test]
    fn truncate_tool_output_keeps_head_and_tail_with_token_marker() {
        let output = format!(
            "{}\n{}\n{}",
            "h".repeat(4000),
            "-".repeat(8000),
            "t".repeat(2000)
        );
        let truncated = truncate_tool_output(&output, 1_000, 500);

        assert!(truncated.len() < output.len());
        assert!(truncated.starts_with(&"h".repeat(4000)));
        assert!(truncated.ends_with(&"t".repeat(2000)));
        assert!(truncated.contains("tokens elided"));
    }

    #[test]
    fn truncate_tool_output_passthrough_below_threshold() {
        let output = "small output";
        assert_eq!(truncate_tool_output(output, 1_000, 500), output);
    }

    #[test]
    fn truncate_tool_output_multibyte_safe() {
        // Emoji + CJK: naive byte slicing would panic here.
        let output = "🦀".repeat(5000) + &"汉".repeat(5000);
        let truncated = truncate_tool_output(&output, 100, 50);

        assert!(truncated.contains("tokens elided"));
        assert!(truncated.chars().count() < output.chars().count());
    }

    #[test]
    fn truncate_tool_output_never_grows_multibyte_output() {
        // 3 000 CJK chars is 9 000 bytes, so the byte-based token estimate
        // (9 000 / 4 = 2 250) clears the 1 500-token budget, while the char
        // count (3 000) sits below the 4 000 + 2 000 char head+tail budget.
        // Slicing by those budgets made the head and tail overlap, so the
        // "truncated" output came back longer than the input.
        let output = "汉".repeat(3_000);
        let truncated = truncate_tool_output(&output, 1_000, 500);

        assert!(
            truncated.chars().count() <= output.chars().count(),
            "truncation grew the output: {} chars from {}",
            truncated.chars().count(),
            output.chars().count()
        );
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
        let strategy = select_compact_strategy(
            "max_tokens",
            50000,
            &ModelId::sonnet(),
            &CompactConfig::default(),
        );
        assert!(matches!(strategy, CompactStrategy::Reactive));
    }

    #[test]
    fn sonnet_lines_come_from_the_window_arithmetic() {
        let thresholds = CompactThresholds::resolve(&ModelId::sonnet(), &CompactConfig::default());

        // 200_000 − min(20_000, 20_000) − 13_000
        assert_eq!(thresholds.auto, 167_000);
        assert_eq!(thresholds.warn, 147_000);
        assert_eq!(thresholds.blocked, 197_000);
    }

    #[test]
    fn select_strategy_over_the_compaction_line_is_auto() {
        let strategy = select_compact_strategy(
            "end_turn",
            170_000,
            &ModelId::sonnet(),
            &CompactConfig::default(),
        );
        assert!(matches!(strategy, CompactStrategy::Auto));
    }

    #[test]
    fn select_strategy_below_the_lines_is_the_lightest_pass() {
        // Compacting at half the window is what this used to do; a session at
        // 110K on a 200K window still has room and must not be truncated.
        for tokens in [50_000, 110_000, 146_999] {
            let strategy = select_compact_strategy(
                "end_turn",
                tokens,
                &ModelId::sonnet(),
                &CompactConfig::default(),
            );
            assert!(
                matches!(strategy, CompactStrategy::Micro),
                "{tokens} tokens gave {strategy:?}"
            );
        }
    }

    #[test]
    fn select_strategy_above_the_blocked_line_keeps_only_what_fits() {
        let strategy = select_compact_strategy(
            "end_turn",
            198_000,
            &ModelId::sonnet(),
            &CompactConfig::default(),
        );
        assert!(matches!(strategy, CompactStrategy::Snip));
    }

    /// An unrecognised model must not be assumed to have the largest window we
    /// know about. At a conservative 32K window the reserves would consume the
    /// whole window, so the floor applies and the line lands at half of it.
    #[test]
    fn unknown_model_compacts_against_a_conservative_window() {
        let model = ModelId("custom-model".into());
        assert_eq!(model.context_window(), crate::types::UNKNOWN_CONTEXT_WINDOW);

        let thresholds = CompactThresholds::resolve(&model, &CompactConfig::default());
        assert_eq!(thresholds.auto, 16_000);
        assert_eq!(thresholds.warn, 8_000);
        assert_eq!(thresholds.blocked, 29_000);

        // Past the end of that window: keep only what fits.
        let strategy =
            select_compact_strategy("end_turn", 110_000, &model, &CompactConfig::default());
        assert!(matches!(strategy, CompactStrategy::Snip));

        // Just over the compaction line for that window.
        let strategy =
            select_compact_strategy("end_turn", 17_000, &model, &CompactConfig::default());
        assert!(matches!(strategy, CompactStrategy::Auto));

        // Under every line: the lightest pass, not a truncation.
        let strategy =
            select_compact_strategy("end_turn", 5_000, &model, &CompactConfig::default());
        assert!(matches!(strategy, CompactStrategy::Micro));
    }

    #[test]
    fn a_configured_output_reserve_moves_the_line() {
        let config = CompactConfig {
            output_reserve_tokens: 4_000,
            ..CompactConfig::default()
        };
        let thresholds = CompactThresholds::resolve(&ModelId::sonnet(), &config);
        assert_eq!(thresholds.auto, 200_000 - 4_000 - 13_000);
    }

    #[test]
    fn the_output_reserve_is_capped() {
        let config = CompactConfig {
            output_reserve_tokens: 100_000,
            ..CompactConfig::default()
        };
        let thresholds = CompactThresholds::resolve(&ModelId::sonnet(), &config);
        assert_eq!(
            thresholds.auto,
            200_000 - CompactConfig::MAX_OUTPUT_RESERVE_TOKENS - 13_000
        );
    }

    #[test]
    fn the_warning_line_keeps_a_band_on_a_small_window() {
        // 20K below a 16K line would leave nothing on either side of it.
        let thresholds =
            CompactThresholds::resolve(&ModelId("custom-model".into()), &CompactConfig::default());
        assert_eq!(thresholds.warn, thresholds.auto / 2);
    }

    /// A gateway alias is unknown by default (32K), so it compacts early. A
    /// per-model override must move every line and change the strategy chosen
    /// for the same token count.
    #[test]
    fn a_context_window_override_moves_the_compaction_lines() {
        let model = ModelId("gateway-alias".into());
        let config = CompactConfig::default();

        let defaults = CompactThresholds::resolve_for_window(
            &config,
            ModelId::table_context_window_for(&model.0),
        );
        assert_eq!(defaults.auto, 16_000);
        assert_eq!(defaults.warn, 8_000);
        assert_eq!(defaults.blocked, 29_000);

        let resolver = ContextWindowResolver::new(
            crate::types::ContextWindowOverrides::from_env_vars([(
                "CODEREX_CONTEXT_WINDOW_GATEWAY_ALIAS",
                "64000",
            )]),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let overridden = CompactThresholds::resolve_with(&model, &config, &resolver);
        assert_eq!(overridden.auto, config.resolve_auto_threshold(64_000));
        assert_eq!(overridden.warn, config.resolve_warning_threshold(64_000));
        assert_eq!(overridden.blocked, config.resolve_blocked_threshold(64_000));
        assert!(overridden.auto > defaults.auto);

        // 20K tokens: past the 16K line before, still below the 31K line with
        // the override.
        let default_strategy = select_compact_strategy("end_turn", 20_000, &model, &config);
        assert!(matches!(default_strategy, CompactStrategy::Auto));
        let overridden_strategy =
            select_compact_strategy_with("end_turn", 20_000, &model, &config, &resolver);
        assert!(matches!(overridden_strategy, CompactStrategy::Micro));
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
        // 200K window − 20K output reserve − 13K buffer
        assert_eq!(cfg.resolve_auto_threshold(200_000), 167_000);
        // Unknown window falls back to configured value.
        assert_eq!(cfg.resolve_auto_threshold(0), cfg.auto_compact_threshold);
    }

    #[test]
    fn resolve_blocked_threshold_leaves_room_for_the_last_request() {
        let cfg = crate::prompt::CompactConfig::default();
        assert_eq!(cfg.resolve_blocked_threshold(200_000), 197_000);
        assert_eq!(
            cfg.resolve_blocked_threshold(0),
            cfg.reactive_compact_threshold
        );
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

    // ─── Cache TTL trigger ─────────────────────────────────────────────────

    /// The wake point is 90% of the TTL, or 30s early — whichever comes first.
    /// For the 5-minute default both rules name the same instant.
    #[test]
    fn the_cache_ttl_wake_point_is_the_earlier_of_the_two_rules() {
        use std::time::Duration;
        assert_eq!(
            cache_ttl_wake_after(Duration::from_secs(300), Duration::from_secs(30)),
            Duration::from_secs(270)
        );
        // A long TTL: 90% is earlier than TTL − 30s, so 90% wins.
        assert_eq!(
            cache_ttl_wake_after(Duration::from_secs(3600), Duration::from_secs(30)),
            Duration::from_secs(3240)
        );
        // A TTL shorter than the early exit falls back to 90% rather than
        // waking at zero, which would call every request cold.
        assert_eq!(
            cache_ttl_wake_after(Duration::from_secs(10), Duration::from_secs(30)),
            Duration::from_secs(9)
        );
        assert_eq!(
            cache_ttl_wake_after(Duration::ZERO, Duration::from_secs(30)),
            Duration::ZERO
        );
    }

    /// The trigger fires at the wake point and not before, and a zero TTL
    /// means "no cache" rather than "always cold".
    #[test]
    fn the_cache_ttl_trigger_fires_at_the_wake_point() {
        use std::time::Duration;
        let ttl = Duration::from_secs(300);
        let early = Duration::from_secs(30);

        assert!(!should_compact_on_cache_ttl(
            Duration::from_secs(269),
            ttl,
            early
        ));
        assert!(should_compact_on_cache_ttl(
            Duration::from_secs(270),
            ttl,
            early
        ));
        assert!(should_compact_on_cache_ttl(
            Duration::from_secs(3600),
            ttl,
            early
        ));
        assert!(!should_compact_on_cache_ttl(
            Duration::from_secs(3600),
            Duration::ZERO,
            early
        ));
    }
}
