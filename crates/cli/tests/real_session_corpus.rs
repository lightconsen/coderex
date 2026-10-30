//! A corpus of real Claude Code sessions.
//!
//! The engine's context paths — import, compaction, loop detection, transcript
//! round-trip — are usually tested on conversations this repository composed.
//! Those are tidy: uniform turns, short tool outputs, no sidechains. Real
//! sessions are not, and the bugs that matter (a tool result whose call was
//! compacted away, a detector that fires on legitimate repetition) only show up
//! in the messy ones.
//!
//! This file reads that corpus from the machine it runs on:
//!
//! - `CODEREX_REAL_SESSIONS_DIR`, defaulting to `~/.claude/projects`;
//! - the same JSONL transcripts Claude Code writes, one entry per line;
//! - the longest sessions with at least a dozen assistant turns.
//!
//! The corpus is **not** committed: it is private conversation data, and it is
//! large. On a machine without it (CI included) every test here prints why and
//! passes, so the suite is still honest about what it did and did not run. Run
//! it with `--nocapture` to see the report:
//!
//! ```bash
//! cargo test -p coderex-cli --test real_session_corpus -- --nocapture
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use coderex_core::PromptCompiler;
use coderex_core::compact;
use coderex_core::loop_detector::{LoopDetection, LoopDetector};
use coderex_core::types::{Message, MessageContent, Role, ThinkingBlock, ToolCall, ToolResult};
use coderex_memory::transcript::TranscriptStore;

/// How many sessions the corpus keeps.
const MAX_SESSIONS: usize = 20;
/// How many files are examined looking for the longest ones.
const MAX_CANDIDATES: usize = 400;
/// A session longer than this is truncated: compaction over hundreds of
/// thousands of messages would dominate the suite's runtime without testing
/// anything the first few thousand do not.
const MAX_MESSAGES_PER_SESSION: usize = 4000;
/// A session must have at least this many assistant turns to count.
const MIN_ASSISTANT_TURNS: usize = 12;

/// One assistant turn: its tool calls and their results.
#[derive(Debug, Default)]
struct Turn {
    calls: Vec<ToolCall>,
    results: Vec<ToolResult>,
}

/// One imported session.
#[derive(Debug)]
struct Session {
    path: PathBuf,
    /// CodeRex messages, in conversation order.
    messages: Vec<Message>,
    /// One entry per assistant turn: the calls the model made in it and what
    /// they returned. An empty call list is a turn with no tool calls, which
    /// is what the engine records and what breaks a run of repeated calls.
    turns: Vec<Turn>,
    /// Tool calls and tool results seen in the source, for the loss check.
    tool_calls: usize,
    tool_results: usize,
}

/// The corpus, or `None` when this machine has none.
fn corpus() -> Option<Vec<Session>> {
    let root = match std::env::var("CODEREX_REAL_SESSIONS_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => home::home_dir()?.join(".claude").join("projects"),
    };
    if !root.is_dir() {
        eprintln!(
            "skipping: no transcript corpus at {} (set CODEREX_REAL_SESSIONS_DIR)",
            root.display()
        );
        return None;
    }

    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    collect_jsonl(&root, &mut files);
    if files.is_empty() {
        eprintln!("skipping: {} holds no .jsonl transcripts", root.display());
        return None;
    }
    // The longest sessions are the ones worth replaying; file size is a cheap
    // proxy that avoids reading all of them first.
    files.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    files.truncate(MAX_CANDIDATES);

    let mut sessions: Vec<Session> = files
        .iter()
        .filter_map(|(_, path)| import_session(path))
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.messages.len()));
    sessions.truncate(MAX_SESSIONS);
    if sessions.is_empty() {
        eprintln!(
            "skipping: no session under {} has {MIN_ASSISTANT_TURNS} assistant turns",
            root.display()
        );
        return None;
    }
    let turns: usize = sessions.iter().map(|session| session.turns.len()).sum();
    let messages: usize = sessions.iter().map(|session| session.messages.len()).sum();
    let calls: usize = sessions.iter().map(|session| session.tool_calls).sum();
    eprintln!(
        "corpus: {} sessions, {messages} messages, {turns} turns, {calls} tool calls from {}",
        sessions.len(),
        root.display()
    );
    Some(sessions)
}

/// Every `.jsonl` file below `dir`, with its size.
fn collect_jsonl(dir: &Path, out: &mut Vec<(u64, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let size = entry.metadata().map_or(0, |meta| meta.len());
            out.push((size, path));
        }
    }
}

/// Import one Claude Code transcript, or `None` when it is too short.
fn import_session(path: &Path) -> Option<Session> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut messages = Vec::new();
    let mut turns: Vec<Turn> = Vec::new();
    let mut tool_calls = 0;
    let mut tool_results = 0;
    // Tool results arrive as a separate user entry after the assistant turn
    // asked for them; the engine closes a turn when the results are in.
    let mut open_turn: Option<Turn> = None;

    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // Sidechains are sub-agent conversations and `isMeta` entries are
        // local-command bookkeeping; neither is part of the main conversation.
        if entry
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            || entry.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true)
        {
            continue;
        }
        let kind = entry.get("type").and_then(serde_json::Value::as_str);
        let Some(message) = entry.get("message") else {
            continue;
        };
        let Some(role) = message.get("role").and_then(serde_json::Value::as_str) else {
            continue;
        };

        match (kind, role) {
            (Some("assistant"), "assistant") => {
                let previous = open_turn.take();
                let mut calls = Vec::new();
                for block in content_blocks(message) {
                    match block.get("type").and_then(serde_json::Value::as_str) {
                        Some("text") => {
                            if let Some(text) =
                                block.get("text").and_then(serde_json::Value::as_str)
                                && !text.trim().is_empty()
                            {
                                messages.push(Message {
                                    role: Role::Assistant,
                                    content: MessageContent::Text(text.to_string()),
                                });
                            }
                        }
                        Some("thinking") => {
                            messages.push(Message {
                                role: Role::Assistant,
                                content: MessageContent::Thinking(ThinkingBlock {
                                    thinking: block
                                        .get("thinking")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    signature: block
                                        .get("signature")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                }),
                            });
                        }
                        Some("tool_use") => {
                            let Some(id) = block.get("id").and_then(serde_json::Value::as_str)
                            else {
                                continue;
                            };
                            let call = ToolCall {
                                id: id.to_string(),
                                name: block
                                    .get("name")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                input: block.get("input").cloned().unwrap_or_default(),
                            };
                            tool_calls += 1;
                            calls.push(call.clone());
                            messages.push(Message {
                                role: Role::Assistant,
                                content: MessageContent::ToolCall(call),
                            });
                        }
                        _ => {}
                    }
                }
                if let Some(previous) = previous {
                    turns.push(previous);
                }
                open_turn = Some(Turn {
                    calls,
                    results: Vec::new(),
                });
            }
            (Some("user"), "user") => {
                let mut results = Vec::new();
                match message.get("content") {
                    Some(serde_json::Value::String(text)) => {
                        if !text.trim().is_empty() {
                            messages.push(Message {
                                role: Role::User,
                                content: MessageContent::Text(text.clone()),
                            });
                        }
                    }
                    Some(serde_json::Value::Array(blocks)) => {
                        for block in blocks {
                            match block.get("type").and_then(serde_json::Value::as_str) {
                                Some("text") => {
                                    if let Some(text) =
                                        block.get("text").and_then(serde_json::Value::as_str)
                                        && !text.trim().is_empty()
                                    {
                                        messages.push(Message {
                                            role: Role::User,
                                            content: MessageContent::Text(text.to_string()),
                                        });
                                    }
                                }
                                Some("tool_result") => {
                                    let Some(id) = block
                                        .get("tool_use_id")
                                        .and_then(serde_json::Value::as_str)
                                    else {
                                        continue;
                                    };
                                    tool_results += 1;
                                    let result = ToolResult {
                                        tool_call_id: id.to_string(),
                                        output: tool_result_text(block.get("content")),
                                        is_error: block
                                            .get("is_error")
                                            .and_then(serde_json::Value::as_bool)
                                            .unwrap_or(false),
                                        images: Vec::new(),
                                    };
                                    results.push(result.clone());
                                    messages.push(Message {
                                        role: Role::User,
                                        content: MessageContent::ToolResult(result),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                // The user's answer closes the turn; a turn with no calls is
                // recorded as such, exactly like the engine's close path.
                if let Some(mut previous) = open_turn.take() {
                    previous.results.clone_from(&results);
                    turns.push(previous);
                }
            }
            _ => {}
        }
        if messages.len() >= MAX_MESSAGES_PER_SESSION {
            break;
        }
    }
    if let Some(previous) = open_turn {
        turns.push(previous);
    }

    let assistant_turns = turns.iter().filter(|turn| !turn.calls.is_empty()).count();
    if assistant_turns < MIN_ASSISTANT_TURNS {
        return None;
    }
    Some(Session {
        path: path.to_path_buf(),
        messages,
        turns,
        tool_calls,
        tool_results,
    })
}

/// The content blocks of a message, whether it is a string or an array.
fn content_blocks(message: &serde_json::Value) -> Vec<&serde_json::Value> {
    match message.get("content") {
        Some(serde_json::Value::Array(blocks)) => blocks.iter().collect(),
        _ => Vec::new(),
    }
}

/// Flatten a tool result's content (a string, or an array of text blocks).
fn tool_result_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Every tool call id in `messages`, in order.
fn tool_call_ids(messages: &[Message]) -> HashSet<&str> {
    messages
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn real_sessions_import_without_losing_tool_calls() {
    let Some(sessions) = corpus() else {
        return;
    };
    for session in &sessions {
        let imported_calls = session
            .messages
            .iter()
            .filter(|message| matches!(message.content, MessageContent::ToolCall(_)))
            .count();
        let imported_results = session
            .messages
            .iter()
            .filter(|message| matches!(message.content, MessageContent::ToolResult(_)))
            .count();
        assert_eq!(
            imported_calls,
            session.tool_calls,
            "{}: tool calls were lost on import",
            session.path.display()
        );
        assert_eq!(
            imported_results,
            session.tool_results,
            "{}: tool results were lost on import",
            session.path.display()
        );
        assert!(
            !session.messages.is_empty(),
            "{} imported no messages",
            session.path.display()
        );
    }
    eprintln!(
        "imported {} sessions without dropping a tool call",
        sessions.len()
    );
}

#[test]
fn compaction_keeps_every_tool_result_next_to_its_call() {
    let Some(sessions) = corpus() else {
        return;
    };
    for session in &sessions {
        let before = PromptCompiler::estimate_message_tokens(&session.messages);
        if before == 0 {
            continue;
        }
        // Compact down to a quarter of the original budget, which is well past
        // what a threshold-triggered compaction would do.
        let target = (before / 4).max(1);
        let result = compact::auto_compact(&session.messages, target);
        let after = PromptCompiler::estimate_message_tokens(&result.messages);
        assert!(
            after <= before,
            "{}: compaction grew the context ({before} -> {after})",
            session.path.display()
        );

        // A tool result whose call is no longer in the context would be
        // rejected by the provider; the compaction strategies must not split
        // the pair.
        let kept_calls = tool_call_ids(&result.messages);
        for message in &result.messages {
            if let MessageContent::ToolResult(result) = &message.content {
                assert!(
                    kept_calls.contains(result.tool_call_id.as_str()),
                    "{}: kept a result for {} without its call",
                    session.path.display(),
                    result.tool_call_id
                );
            }
        }
        assert!(
            result
                .messages
                .iter()
                .any(|message| message.role == Role::User),
            "{}: compaction removed every user message",
            session.path.display()
        );
    }
    eprintln!(
        "compacted {} real sessions and kept every tool result paired",
        sessions.len()
    );
}

#[test]
fn the_loop_detector_stays_quiet_on_real_sessions() {
    let Some(sessions) = corpus() else {
        return;
    };
    let mut false_positives = Vec::new();
    for session in &sessions {
        let mut detector = LoopDetector::new();
        for turn in &session.turns {
            if turn.calls.is_empty() {
                detector.record_empty_turn();
            } else {
                detector.record_turn_with_results(&turn.calls, &turn.results);
            }
            if let LoopDetection::LoopDetected { description, .. } = detector.check() {
                false_positives.push((session.path.clone(), description));
                break;
            }
        }
    }
    assert!(
        false_positives.is_empty(),
        "the detector fired on a real session: {false_positives:?}"
    );
    eprintln!(
        "the loop detector stayed quiet across {} real sessions",
        sessions.len()
    );
}

#[test]
fn a_real_session_round_trips_through_the_transcript_store() {
    let Some(sessions) = corpus() else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let store = TranscriptStore::new(dir.path().to_path_buf());
    let session = &sessions[0];
    // The store keys by session id; use the file stem of the transcript.
    let id = "real-session-corpus";
    for message in &session.messages {
        store.append(id, message).expect("append");
    }
    let read_back = store.read_all(id).expect("read_all");
    assert_eq!(
        read_back.len(),
        session.messages.len(),
        "the store dropped messages"
    );
    for (index, (written, read)) in session.messages.iter().zip(read_back.iter()).enumerate() {
        assert_eq!(
            std::mem::discriminant(&written.content),
            std::mem::discriminant(&read.content),
            "message {index} changed kind on the round trip"
        );
    }
    eprintln!(
        "round-tripped {} messages through the transcript store",
        read_back.len()
    );
}
