//! What one session was — read out of its transcript.
//!
//! A transcript is a JSONL file of messages, one per line. Everything the
//! report knows starts here: what the person asked for, what tools ran, what
//! failed, and how much of the conversation was spent on each of those.
//!
//! Reading is defensive on purpose. A transcript is written by a process that
//! can be killed mid-line, so a half-written last line is skipped rather than
//! failing the whole session, and a session that cannot be read at all is
//! reported as unreadable rather than silently dropped from the totals.

use coderex_core::types::{Message, MessageContent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Longest first prompt kept, in characters.
pub const PROMPT_EXCERPT_CHARS: usize = 200;

/// What one session contained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFacts {
    /// Session id, taken from the file name.
    pub session_id: String,
    /// When the transcript was last written, as an RFC 3339 timestamp, when
    /// the filesystem will say.
    pub last_active: Option<String>,
    /// How many messages the session holds.
    pub messages: usize,
    /// Messages by role: `user`, `assistant`, `system`.
    pub by_role: BTreeMap<String, usize>,
    /// What the person opened with, trimmed.
    pub first_prompt: String,
    /// How many times each tool was called.
    pub tool_calls: BTreeMap<String, usize>,
    /// How many tool results came back as errors, by tool name.
    pub tool_errors: BTreeMap<String, usize>,
    /// How many times the conversation was compacted.
    pub compactions: usize,
    /// Whether anything failed to parse.
    pub unreadable_lines: usize,
}

impl SessionFacts {
    /// Total tool calls.
    #[must_use]
    pub fn total_tool_calls(&self) -> usize {
        self.tool_calls.values().sum()
    }

    /// Total tool calls that failed.
    #[must_use]
    pub fn total_tool_errors(&self) -> usize {
        self.tool_errors.values().sum()
    }

    /// The share of tool calls that failed, or `None` when nothing ran.
    #[must_use]
    // Display ratio; call counts are far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    pub fn error_rate(&self) -> Option<f64> {
        let calls = self.total_tool_calls();
        (calls > 0).then(|| self.total_tool_errors() as f64 / calls as f64)
    }

    /// Messages the person sent.
    #[must_use]
    pub fn user_messages(&self) -> usize {
        self.by_role.get("user").copied().unwrap_or(0)
    }

    /// Read one transcript file.
    ///
    /// # Errors
    ///
    /// Fails only when the file cannot be opened; unreadable lines inside it
    /// are counted in `unreadable_lines` instead.
    pub fn read(path: &Path) -> Result<Self, std::io::Error> {
        let session_id = path.file_stem().map_or_else(
            || path.display().to_string(),
            |stem| stem.to_string_lossy().to_string(),
        );

        let text = std::fs::read_to_string(path)?;
        Ok(Self::from_jsonl(&session_id, &text, last_active(path)))
    }

    /// Read a transcript that is already in memory.
    #[must_use]
    pub fn from_jsonl(session_id: &str, text: &str, last_active: Option<String>) -> Self {
        let mut facts = Self {
            session_id: session_id.to_string(),
            last_active,
            messages: 0,
            by_role: BTreeMap::new(),
            first_prompt: String::new(),
            tool_calls: BTreeMap::new(),
            tool_errors: BTreeMap::new(),
            compactions: 0,
            unreadable_lines: 0,
        };

        // A tool result names its call by id; the name lives on the call. This
        // is how a failure gets attributed to the tool that failed.
        let mut call_names: BTreeMap<String, String> = BTreeMap::new();

        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Message>(line) else {
                // A killed process leaves a half-line behind; the message it
                // was going to be simply never happened.
                facts.unreadable_lines += 1;
                continue;
            };

            facts.messages += 1;
            let role = role_name(&message);
            *facts.by_role.entry(role.to_string()).or_insert(0) += 1;

            match &message.content {
                MessageContent::Text(text) => {
                    if role == "user" && facts.first_prompt.is_empty() {
                        facts.first_prompt = excerpt(text, PROMPT_EXCERPT_CHARS);
                    }
                    if text.contains("<coderex_compact_boundary>") {
                        facts.compactions += 1;
                    }
                }
                MessageContent::ToolCall(call) => {
                    *facts.tool_calls.entry(call.name.clone()).or_insert(0) += 1;
                    call_names.insert(call.id.clone(), call.name.clone());
                }
                MessageContent::ToolResult(result) if result.is_error => {
                    let name = call_names
                        .get(&result.tool_call_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    *facts.tool_errors.entry(name).or_insert(0) += 1;
                }
                // Thinking is provider state, not visible assistant output.
                // The content enum is open: a transcript written by a newer
                // CodeRex may hold something this one does not know. It still
                // counts as a message; it just has nothing to add.
                _ => {}
            }
        }

        facts
    }
}

/// Whether a path looks like a transcript.
#[must_use]
pub fn is_transcript(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

/// Read every transcript in a directory.
///
/// # Errors
///
/// Fails if the directory cannot be listed. Individual sessions that cannot be
/// read are logged and left out.
pub fn read_dir(dir: &Path) -> Result<Vec<SessionFacts>, std::io::Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !is_transcript(&path) {
            continue;
        }
        match SessionFacts::read(&path) {
            Ok(facts) => sessions.push(facts),
            Err(error) => tracing::warn!(path = %path.display(), "skipping a transcript: {error}"),
        }
    }
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(sessions)
}

const fn role_name(message: &Message) -> &'static str {
    match message.role {
        coderex_core::types::Role::User => "user",
        coderex_core::types::Role::Assistant => "assistant",
        coderex_core::types::Role::System => "system",
        coderex_core::types::Role::Tool => "tool",
        // Open enum: a role this version does not know is still a message,
        // and naming it keeps it out of the counts it does not belong in.
        _ => "other",
    }
}

fn last_active(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let time: chrono::DateTime<chrono::Utc> = modified.into();
    Some(time.to_rfc3339())
}

fn excerpt(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let mut trimmed: String = collapsed.chars().take(limit).collect();
    trimmed.push('…');
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::{Role, ToolCall, ToolResult};

    fn line(message: &Message) -> String {
        serde_json::to_string(message).unwrap()
    }

    #[test]
    fn a_session_is_read_into_counts() {
        let messages = [
            Message::user("  fix   the parser\nplease  "),
            Message {
                role: Role::Assistant,
                content: MessageContent::ToolCall(ToolCall {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "src/a.rs"}),
                }),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    tool_call_id: "t1".into(),
                    output: "file contents".into(),
                    is_error: false,
                    images: Vec::new(),
                }),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::ToolCall(ToolCall {
                    id: "t2".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "cargo test"}),
                }),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    tool_call_id: "t2".into(),
                    output: "error: test failed".into(),
                    is_error: true,
                    images: Vec::new(),
                }),
            },
            Message::system("<coderex_compact_boundary>\nsummarized\n</coderex_compact_boundary>"),
        ];

        let text = messages.iter().map(line).collect::<Vec<_>>().join("\n");
        let facts = SessionFacts::from_jsonl("sess_1", &text, None);

        assert_eq!(facts.session_id, "sess_1");
        assert_eq!(facts.messages, 6);
        assert_eq!(facts.user_messages(), 1);
        assert_eq!(facts.by_role["assistant"], 2);
        assert_eq!(facts.first_prompt, "fix the parser please");
        assert_eq!(facts.tool_calls["Read"], 1);
        assert_eq!(facts.tool_calls["Bash"], 1);
        assert_eq!(facts.total_tool_calls(), 2);
        // The failure is attributed to the tool that made the call.
        assert_eq!(facts.tool_errors["Bash"], 1);
        assert_eq!(facts.total_tool_errors(), 1);
        assert_eq!(facts.compactions, 1);
        assert_eq!(facts.unreadable_lines, 0);
    }

    #[test]
    fn a_half_written_last_line_is_skipped_not_fatal() {
        let text = format!(
            "{}\n{{\"role\":\"User\",\"content\":{{\"Text\":\"cut off mid",
            line(&Message::user("a real message"))
        );
        let facts = SessionFacts::from_jsonl("sess_2", &text, None);

        assert_eq!(facts.messages, 1);
        assert_eq!(facts.unreadable_lines, 1);
        assert_eq!(facts.first_prompt, "a real message");
    }

    #[test]
    fn a_failure_with_no_matching_call_is_still_counted() {
        let text = line(&Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                tool_call_id: "gone".into(),
                output: "failed".into(),
                is_error: true,
                images: Vec::new(),
            }),
        });
        let facts = SessionFacts::from_jsonl("sess_3", &text, None);
        assert_eq!(facts.tool_errors["unknown"], 1);
    }

    #[test]
    fn an_excerpt_collapses_whitespace_and_stops_at_the_limit() {
        let long = "word ".repeat(100);
        let facts = SessionFacts::from_jsonl("s", &line(&Message::user(&long)), None);
        assert!(facts.first_prompt.ends_with('…'), "{}", facts.first_prompt);
        assert_eq!(facts.first_prompt.chars().count(), PROMPT_EXCERPT_CHARS + 1);
        assert!(!facts.first_prompt.contains("  "));
    }

    #[test]
    fn an_empty_transcript_reads_as_empty_rather_than_failing() {
        let facts = SessionFacts::from_jsonl("empty", "", None);
        assert_eq!(facts.messages, 0);
        assert_eq!(facts.error_rate(), None);
        assert!(facts.first_prompt.is_empty());
    }

    #[test]
    fn the_error_rate_is_only_defined_once_something_ran() {
        let mut facts = SessionFacts::from_jsonl("s", "", None);
        assert_eq!(facts.error_rate(), None);

        facts.tool_calls.insert("Bash".into(), 4);
        facts.tool_errors.insert("Bash".into(), 1);
        assert_eq!(facts.error_rate(), Some(0.25));
    }

    #[test]
    fn a_directory_of_transcripts_is_read_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.jsonl"), line(&Message::user("second"))).unwrap();
        std::fs::write(dir.path().join("a.jsonl"), line(&Message::user("first"))).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a transcript").unwrap();

        let sessions = read_dir(dir.path()).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "a");
        assert_eq!(sessions[0].first_prompt, "first");
        assert_eq!(sessions[1].session_id, "b");
    }

    #[test]
    fn a_directory_that_is_not_there_reads_as_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(read_dir(&missing).unwrap().is_empty());
    }
}
