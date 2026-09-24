//! Compact integration tests — covering all 4 strategies, boundary markers,
//! strategy selection, and dead-loop protection.
//!
//! These tests correspond to the equivalence test plan Module 7:
//! 7.1 micro compact, 7.2 auto compact, 7.3 reactive compact,
//! 7.4 snip compact, 7.5 compact boundary, 7.6 dead loop protection

use coderex_core::compact::{
    CompactResult, CompactStrategy, MicroCompactParams, auto_compact, compact_boundary_message,
    insert_compact_boundary, micro_compact, reactive_compact, select_compact_strategy,
    should_auto_compact, should_reactive_compact, snip_compact,
};
use coderex_core::stop_hook::{
    StopHookContext, StopHookDecision, evaluate_stop_hook, should_proceed_with_compact,
};
use coderex_core::types::{Message, MessageContent, ModelId, Role, ToolCall, ToolResult};

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn make_user(text: &str) -> Message {
    Message::user(text)
}

fn make_assistant(text: &str) -> Message {
    Message::assistant(text)
}

fn make_tool_result(output: &str) -> Message {
    Message::tool(ToolResult {
        tool_call_id: "tc_test".into(),
        output: output.to_string(),
        is_error: false,
    })
}

/// A paired assistant ToolCall + tool result exchange, as produced by a
/// real turn — `micro_compact` resolves the tool name from the ToolCall side.
fn make_tool_exchange(id: &str, name: &str, output: &str) -> Vec<Message> {
    vec![
        Message {
            role: Role::Assistant,
            content: MessageContent::ToolCall(ToolCall {
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

fn make_long_messages(count: usize) -> Vec<Message> {
    let mut messages = Vec::new();
    messages.push(make_user("initial query"));
    for i in 0..count {
        messages.push(make_assistant(&format!("response {}", i).repeat(50)));
        messages.push(make_user(&format!("follow-up {}", i)));
        messages.push(make_tool_result(&format!("tool output {}", i).repeat(100)));
    }
    messages
}

// ═══════════════════════════════════════════════════════════════════════════
//  7.1 Micro Compact — shrink early tool results, preserve recent + identity
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn micro_compact_summarizes_early_tool_results() {
    let mut messages = Vec::new();
    messages.extend(make_tool_exchange("tc_1", "read", &"x".repeat(40_000)));
    messages.extend(make_tool_exchange("tc_2", "read", &"y".repeat(40_000)));
    messages.extend(make_tool_exchange("tc_3", "read", "recent"));

    let result = micro_compact(
        &messages,
        MicroCompactParams {
            keep_recent: 1,
            min_result_tokens: 100,
            ..MicroCompactParams::default()
        },
    );

    assert_eq!(result.strategy, CompactStrategy::Micro);
    assert!(result.tokens_removed > 0);
    // Last tool result preserved
    let last = &result.messages[5];
    assert!(matches!(last.content, MessageContent::ToolResult(_)));
    // Earlier ones keep their identity: Tool role, same tool_call_id,
    // shrunk output with elision marker.
    for (i, id) in [(1, "tc_1"), (3, "tc_2")] {
        assert!(matches!(result.messages[i].role, Role::Tool));
        if let MessageContent::ToolResult(tr) = &result.messages[i].content {
            assert_eq!(tr.tool_call_id, id);
            assert!(tr.output.contains("tokens elided"));
        } else {
            panic!("expected tool result at index {i}");
        }
    }
}

#[test]
fn micro_compact_preserves_pairing() {
    let mut messages = Vec::new();
    messages.extend(make_tool_exchange("tc_1", "read", &"x".repeat(40_000)));
    messages.extend(make_tool_exchange("tc_2", "read", "recent"));

    let result = micro_compact(
        &messages,
        MicroCompactParams {
            keep_recent: 1,
            min_result_tokens: 100,
            ..MicroCompactParams::default()
        },
    );

    // The assistant tool_use id still matches the tool_result tool_call_id
    // after compaction — provider pairing survives.
    if let MessageContent::ToolCall(tc) = &result.messages[0].content {
        if let MessageContent::ToolResult(tr) = &result.messages[1].content {
            assert_eq!(tc.id, tr.tool_call_id);
        } else {
            panic!("expected tool result");
        }
    } else {
        panic!("expected tool call");
    }
}

#[test]
fn micro_compact_unknown_tool_untouched() {
    let mut messages = Vec::new();
    messages.extend(make_tool_exchange(
        "tc_1",
        "custom_remote_tool",
        &"x".repeat(40_000),
    ));
    messages.extend(make_tool_exchange("tc_2", "read", "recent"));

    let result = micro_compact(
        &messages,
        MicroCompactParams {
            keep_recent: 1,
            min_result_tokens: 100,
            ..MicroCompactParams::default()
        },
    );

    // Non-whitelisted tool output kept verbatim.
    assert_eq!(result.tokens_removed, 0);
    if let MessageContent::ToolResult(tr) = &result.messages[1].content {
        assert_eq!(tr.output, "x".repeat(40_000));
    } else {
        panic!("expected tool result");
    }
}

#[test]
fn micro_compact_no_op_when_few_tool_results() {
    let messages = vec![make_user("hello"), make_tool_result("only result")];

    let result = micro_compact(&messages, MicroCompactParams::default());
    assert_eq!(result.tokens_removed, 0);
    assert!(matches!(
        result.messages[1].content,
        MessageContent::ToolResult(_)
    ));
}

#[test]
fn micro_compact_empty_messages() {
    let result = micro_compact(&[], MicroCompactParams::default());
    assert_eq!(result.tokens_removed, 0);
    assert!(result.messages.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  7.2 Auto Compact — triggers at threshold, removes assistant text
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn auto_compact_triggers_above_threshold() {
    let mut messages = Vec::new();
    messages.push(make_user("start"));
    messages.push(make_assistant(&"x".repeat(5000)));
    messages.push(make_assistant(&"y".repeat(5000)));
    messages.push(make_user("end"));

    let result = auto_compact(&messages, 100);
    assert_eq!(result.strategy, CompactStrategy::Auto);
    // Should have removed some assistant text messages
    let assistant_count = result
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .count();
    assert!(
        assistant_count <= 1,
        "expected at most 1 assistant message after auto compact"
    );
}

#[test]
fn auto_compact_does_not_trigger_below_threshold() {
    let messages = vec![make_user("hello"), make_assistant("world")];

    // Use a very high threshold so auto compact should not trigger
    let result = auto_compact(&messages, 100_000);
    // The result should keep all messages since we're way under threshold
    assert!(result.messages.len() >= 2);
}

#[test]
fn should_auto_compact_returns_true_above_threshold() {
    assert!(should_auto_compact(
        1000,
        500,
        &coderex_core::budget::TokenBudget::with_defaults()
    ));
}

#[test]
fn should_auto_compact_returns_false_below_threshold() {
    assert!(!should_auto_compact(
        100,
        500,
        &coderex_core::budget::TokenBudget::with_defaults()
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  7.3 Reactive Compact — truncates old turns, preserves first user
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn reactive_compact_preserves_first_user_and_recent_turns() {
    let mut messages = Vec::new();
    messages.push(make_user("initial query"));
    for i in 0..10 {
        messages.push(make_assistant(&format!("response {}", i)));
        messages.push(make_user(&format!("follow-up {}", i)));
    }

    let result = reactive_compact(&messages, 2);

    assert_eq!(result.strategy, CompactStrategy::Reactive);
    assert!(!result.messages.is_empty());
    // First message must be the initial user query
    let first = &result.messages[0];
    assert!(matches!(first.role, Role::User));
    if let MessageContent::Text(s) = &first.content {
        assert_eq!(s, "initial query");
    }
}

#[test]
fn reactive_compact_boundary_inserted_when_gap_exists() {
    let mut messages = Vec::new();
    messages.push(make_user("first"));
    messages.push(make_assistant("a"));
    messages.push(make_user("second"));
    messages.push(make_assistant("b"));
    messages.push(make_user("third"));
    messages.push(make_assistant("c"));
    messages.push(make_user("fourth"));

    let result = reactive_compact(&messages, 1);

    assert!(
        result.boundary_inserted,
        "boundary should be inserted when gap exists"
    );
    assert!(result.boundary_index.is_some());
    // The boundary message should be a system message
    let boundary_idx = result.boundary_index.unwrap();
    let boundary_msg = &result.messages[boundary_idx];
    assert!(matches!(boundary_msg.role, Role::System));
}

#[test]
fn reactive_compact_no_boundary_when_no_gap() {
    let messages = vec![make_user("first"), make_assistant("a"), make_user("second")];

    // keep_recent_turns=1 with only 2 turns -> no truncation -> no boundary
    let result = reactive_compact(&messages, 1);
    assert!(!result.boundary_inserted);
    assert!(result.boundary_index.is_none());
}

#[test]
fn reactive_compact_empty_messages() {
    let result = reactive_compact(&[], 2);
    assert!(result.messages.is_empty());
    assert!(!result.boundary_inserted);
}

#[test]
fn should_reactive_compact_returns_true_on_max_tokens() {
    assert!(should_reactive_compact("max_tokens", 100, 1000));
}

#[test]
fn should_reactive_compact_returns_true_above_threshold() {
    assert!(should_reactive_compact("end_turn", 2000, 1000));
}

#[test]
fn should_reactive_compact_returns_false_when_both_ok() {
    assert!(!should_reactive_compact("end_turn", 100, 1000));
}

// ═══════════════════════════════════════════════════════════════════════════
//  7.4 Snip Compact — truncate at token boundary, preserve first user
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn snip_compact_reduces_to_fit_target() {
    let messages = make_long_messages(10);

    let result = snip_compact(&messages, 500);

    assert_eq!(result.strategy, CompactStrategy::Snip);
    // Should have reduced tokens significantly
    assert!(result.tokens_removed > 0);
}

#[test]
fn snip_compact_preserves_first_user_message() {
    let mut messages = Vec::new();
    messages.push(make_user("must preserve this"));
    messages.push(make_assistant(&"x".repeat(1000)));
    messages.push(make_user("also this"));

    let result = snip_compact(&messages, 50);

    assert!(!result.messages.is_empty());
    let first = &result.messages[0];
    assert!(matches!(first.role, Role::User));
    if let MessageContent::Text(s) = &first.content {
        assert_eq!(s, "must preserve this");
    }
}

#[test]
fn snip_compact_boundary_inserted_when_gap_exists() {
    let mut messages = Vec::new();
    messages.push(make_user("first"));
    for i in 0..20 {
        messages.push(make_assistant(&format!("long response {}", i).repeat(50)));
    }
    messages.push(make_user("last"));

    let result = snip_compact(&messages, 200);

    assert!(
        result.boundary_inserted,
        "snip should insert boundary when truncating"
    );
    assert!(result.boundary_index.is_some());
    // Verify boundary message exists
    let boundary_idx = result.boundary_index.unwrap();
    let boundary_msg = &result.messages[boundary_idx];
    if let MessageContent::Text(s) = &boundary_msg.content {
        assert!(s.contains("<coderex_compact_boundary>"));
    } else {
        panic!("expected text boundary message");
    }
}

#[test]
fn snip_compact_no_op_when_under_target() {
    let messages = vec![make_user("hello"), make_assistant("world")];

    let result = snip_compact(&messages, 100_000);
    assert_eq!(result.tokens_removed, 0);
    assert!(!result.boundary_inserted);
}

#[test]
fn snip_compact_empty_messages() {
    let result = snip_compact(&[], 500);
    assert!(result.messages.is_empty());
    assert!(!result.boundary_inserted);
}

// ═══════════════════════════════════════════════════════════════════════════
//  7.5 Compact Boundary — SDK boundary marker format and insertion
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn compact_boundary_message_format() {
    let msg = compact_boundary_message();
    assert!(matches!(msg.role, Role::System));
    if let MessageContent::Text(s) = &msg.content {
        assert!(s.contains("<coderex_compact_boundary>"));
        assert!(s.contains("</coderex_compact_boundary>"));
        assert!(s.contains("truncated"));
    } else {
        panic!("expected text content");
    }
}

#[test]
fn insert_compact_boundary_at_index() {
    let mut messages = vec![
        make_user("first"),
        make_assistant("second"),
        make_user("third"),
    ];

    insert_compact_boundary(&mut messages, 1);

    assert_eq!(messages.len(), 4);
    assert!(matches!(messages[1].role, Role::System));
    if let MessageContent::Text(s) = &messages[1].content {
        assert!(s.contains("<coderex_compact_boundary>"));
    }
}

#[test]
fn insert_compact_boundary_at_end_when_index_out_of_range() {
    let mut messages = vec![make_user("first"), make_assistant("second")];

    insert_compact_boundary(&mut messages, 100);

    assert_eq!(messages.len(), 3);
    assert!(matches!(messages[2].role, Role::System));
}

#[test]
fn compact_boundary_index_is_correct_in_result() {
    let messages = vec![
        make_user("first"),
        make_assistant("a"),
        make_user("second"),
        make_assistant("b"),
        make_user("third"),
        make_assistant("c"),
        make_user("fourth"),
    ];

    let result = reactive_compact(&messages, 1);

    assert!(result.boundary_inserted);
    let idx = result.boundary_index.expect("should have boundary index");
    // Boundary should come after the first user message
    assert!(idx > 0);
    assert!(matches!(result.messages[idx].role, Role::System));
}

// ═══════════════════════════════════════════════════════════════════════════
//  7.6 Dead Loop Protection — stop hook terminates after max retries
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn stop_hook_terminates_at_max_recovery_retries() {
    let ctx = StopHookContext {
        recovery_count: 3,
        max_recovery_retries: 3,
        estimated_tokens: 190_000,
        compact_strategy: CompactStrategy::Reactive,
        stop_reason: "max_tokens".into(),
    };

    let decision = evaluate_stop_hook(&ctx);
    assert!(
        matches!(decision, StopHookDecision::Terminate { .. }),
        "should terminate when recovery_count >= max_recovery_retries"
    );
    if let StopHookDecision::Terminate { reason } = decision {
        assert!(reason.contains("limit reached"));
    }
}

#[test]
fn stop_hook_allows_continue_below_limit() {
    let ctx = StopHookContext {
        recovery_count: 1,
        max_recovery_retries: 3,
        estimated_tokens: 150_000,
        compact_strategy: CompactStrategy::Auto,
        stop_reason: "end_turn".into(),
    };

    let decision = evaluate_stop_hook(&ctx);
    assert!(
        matches!(decision, StopHookDecision::Continue { .. }),
        "should continue when below retry limit"
    );
}

#[test]
fn should_proceed_returns_false_for_terminate() {
    let terminate = StopHookDecision::Terminate {
        reason: "too many retries".into(),
    };
    assert!(!should_proceed_with_compact(&terminate));
}

#[test]
fn should_proceed_returns_true_for_continue() {
    let cont = StopHookDecision::Continue {
        updated_stop_reason: None,
    };
    assert!(should_proceed_with_compact(&cont));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Strategy Selection — correct strategy chosen for model and token state
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn select_strategy_max_tokens_triggers_reactive() {
    let strategy = select_compact_strategy("max_tokens", 50_000, &ModelId::sonnet(), false);
    assert_eq!(strategy, CompactStrategy::Reactive);
}

#[test]
fn select_strategy_over_reactive_threshold() {
    // Sonnet context window = 200K, reactive threshold = 90% = 180K
    let strategy = select_compact_strategy("end_turn", 190_000, &ModelId::sonnet(), false);
    assert_eq!(strategy, CompactStrategy::Reactive);
}

#[test]
fn select_strategy_over_auto_threshold() {
    // Sonnet context window = 200K, auto threshold = 50% = 100K
    let strategy = select_compact_strategy("end_turn", 110_000, &ModelId::sonnet(), false);
    assert_eq!(strategy, CompactStrategy::Auto);
}

#[test]
fn select_strategy_budget_near_limit_is_micro() {
    let strategy = select_compact_strategy("end_turn", 10_000, &ModelId::sonnet(), true);
    assert_eq!(strategy, CompactStrategy::Micro);
}

#[test]
fn select_strategy_default_is_snip() {
    let strategy = select_compact_strategy("end_turn", 10_000, &ModelId::sonnet(), false);
    assert_eq!(strategy, CompactStrategy::Snip);
}

#[test]
fn select_strategy_small_model_window() {
    // A model with a smaller window (default 200K)
    let model = ModelId("custom-small-model".into());
    let strategy = select_compact_strategy("end_turn", 110_000, &model, false);
    assert_eq!(strategy, CompactStrategy::Auto);
}

#[test]
fn select_strategy_exactly_at_auto_threshold_falls_through() {
    // Sonnet window = 200K, auto = 100K. Exactly at threshold falls through to Snip
    // because select_compact_strategy uses > not >=
    let strategy = select_compact_strategy("end_turn", 100_000, &ModelId::sonnet(), false);
    assert_eq!(strategy, CompactStrategy::Snip);
}

#[test]
fn select_strategy_exactly_at_reactive_threshold_falls_through() {
    // Sonnet window = 200K, reactive = 180K. Exactly at threshold falls through to Auto
    let strategy = select_compact_strategy("end_turn", 180_000, &ModelId::sonnet(), false);
    assert_eq!(strategy, CompactStrategy::Auto);
}

// ═══════════════════════════════════════════════════════════════════════════
//  End-to-end compact flow — chained strategies
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn compact_pipeline_micro_then_auto() {
    let mut messages = Vec::new();
    messages.push(make_user("start"));
    for i in 0..5 {
        messages.extend(make_tool_exchange(
            &format!("tc_{i}"),
            "read",
            &format!("tool output {i}").repeat(2000),
        ));
        messages.push(make_assistant(&format!("response {}", i).repeat(200)));
    }
    messages.push(make_user("end"));

    // First apply micro
    let micro = micro_compact(
        &messages,
        MicroCompactParams {
            keep_recent: 1,
            ..MicroCompactParams::default()
        },
    );
    assert_eq!(micro.strategy, CompactStrategy::Micro);
    assert!(micro.tokens_removed > 0);

    // Then apply auto on the result
    let auto = auto_compact(&micro.messages, 100);
    assert_eq!(auto.strategy, CompactStrategy::Auto);
}

#[test]
fn compact_pipeline_reactive_then_snip() {
    let messages = make_long_messages(15);

    // First reactive to keep only recent turns
    let reactive = reactive_compact(&messages, 3);
    assert_eq!(reactive.strategy, CompactStrategy::Reactive);
    assert!(reactive.boundary_inserted);

    // Then snip to fit a hard token limit
    let snipped = snip_compact(&reactive.messages, 500);
    assert_eq!(snipped.strategy, CompactStrategy::Snip);
}

#[test]
fn compact_preserves_message_count_with_no_tool_results() {
    let messages = vec![
        make_user("hello"),
        make_assistant("world"),
        make_user("how are you"),
        make_assistant("fine"),
    ];

    // Micro with no tool results should be a no-op
    let result = micro_compact(&messages, MicroCompactParams::default());
    assert_eq!(result.tokens_removed, 0);
    assert_eq!(result.messages.len(), 4);
}
