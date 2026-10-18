//! Transcript entry types — what a single line of a `.jsonl` transcript is.
//!
//! A transcript line used to be a bare [`Message`]. That is still true of
//! every line CodeRex writes today, but it leaves two things unsaid: a
//! compaction boundary is a system message with a magic string inside it, and
//! a transcript has no header saying which session, working directory, or
//! model it belongs to.
//!
//! This module makes those explicit without changing what an ordinary line
//! looks like. A [`TranscriptEntry`] is one of:
//!
//! - [`TranscriptEntry::Message`] — a conversation message, written exactly as
//!   before (`{"role": …, "content": …}`), so every transcript on disk still
//!   reads.
//! - [`TranscriptEntry::CompactBoundary`] — where context was compacted. The
//!   engine already emits this as a system message carrying
//!   [`COMPACT_BOUNDARY_MARKER`]; here it is a type rather than a string a
//!   reader has to know to look for.
//! - [`TranscriptEntry::SessionMeta`] — a header naming the session, its
//!   working directory, and its model, so a transcript can be understood on
//!   its own after the file is moved.
//!
//! Non-message entries carry a `"type"` tag; messages do not, which is what
//! keeps old files readable. [`TranscriptEntry::from_message`] classifies a
//! message into the right variant, and [`TranscriptEntry::to_message`] turns
//! it back, so the two forms round-trip through each other.
//!
//! These types are the **read-side** model. [`crate::transcript::TranscriptStore`]
//! writes bare messages only and classifies every line — bare or tagged —
//! through [`TranscriptEntry::from_message`] when it reads. There is no typed
//! write path, because production does not have one.
//!
//! # What is deliberately left out
//!
//! Claude's transcript format has three much larger tables: nine visible
//! message types, twenty-one system subtypes, and twenty-odd attachment
//! types. CodeRex produces none of the system subtypes (no hook results, no
//! turn durations, no API-error records are written to a transcript) and has
//! no attachment concept at all — a file the model read arrives as the output
//! of a `Read` tool call inside a [`MessageContent::ToolResult`], not as a
//! separate attachment entry. Inventing those variants would describe a
//! format CodeRex does not write, so they are not here. If CodeRex grows a
//! hook-result or attachment record, it earns a variant then.

use coderex_core::types::{Message, MessageContent, Role};
use serde::{Deserialize, Serialize};

/// The marker inside a system message that says "context was compacted here".
pub const COMPACT_BOUNDARY_MARKER: &str = "<coderex_compact_boundary>";

/// The marker that closes a compaction boundary message.
pub const COMPACT_BOUNDARY_CLOSE: &str = "</coderex_compact_boundary>";

/// A single line of a transcript.
///
/// There is no `PartialEq`: a `Message` does not have one, and comparing two
/// entries means comparing the two lines they serialize to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptEntry {
    /// A conversation message. Serialized bare, as it always has been.
    Message(Message),
    /// Any entry that is not a plain message, tagged with `"type"`.
    Tagged(TaggedEntry),
}

/// The non-message entries, each carrying a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaggedEntry {
    /// Where the conversation was compacted.
    CompactBoundary(CompactBoundary),
    /// The header naming the session a transcript belongs to.
    SessionMeta(SessionMeta),
}

/// A compaction boundary: the point past which earlier history was removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactBoundary {
    /// When the boundary was written.
    pub at: chrono::DateTime<chrono::Utc>,
    /// The note shown in place of the removed history.
    pub note: String,
}

/// The header naming the session a transcript belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The session id, matching the transcript's file name.
    pub session_id: String,
    /// The working directory the session ran in.
    pub cwd: String,
    /// The model the session ran on, when it is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// When the session started.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// What a transcript entry is, at the granularity CodeRex actually produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A user's text.
    UserText,
    /// The assistant's text.
    AssistantText,
    /// The assistant asking to run a tool.
    AssistantToolCall,
    /// A tool's result coming back.
    ToolResult,
    /// A system message that is not a compaction boundary.
    SystemText,
    /// A compaction boundary.
    CompactBoundary,
    /// A session header.
    SessionMeta,
}

impl EntryKind {
    /// The tag written for this kind, matching the serde discriminator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserText => "user_text",
            Self::AssistantText => "assistant_text",
            Self::AssistantToolCall => "assistant_tool_call",
            Self::ToolResult => "tool_result",
            Self::SystemText => "system_text",
            Self::CompactBoundary => "compact_boundary",
            Self::SessionMeta => "session_meta",
        }
    }

    /// Whether this kind is shown to a person replaying the session.
    ///
    /// A session header is metadata, not something that happened; a boundary
    /// is shown, because a reader has to know the history has a gap in it.
    #[must_use]
    pub const fn is_visible(self) -> bool {
        !matches!(self, Self::SessionMeta)
    }
}

impl TranscriptEntry {
    /// A plain message entry.
    #[must_use]
    pub const fn message(message: Message) -> Self {
        Self::Message(message)
    }

    /// A compaction boundary entry, written now.
    #[must_use]
    pub fn compact_boundary(note: impl Into<String>) -> Self {
        Self::Tagged(TaggedEntry::CompactBoundary(CompactBoundary {
            at: chrono::Utc::now(),
            note: note.into(),
        }))
    }

    /// A session header entry.
    #[must_use]
    pub fn session_meta(
        session_id: impl Into<String>,
        cwd: impl Into<String>,
        model: Option<String>,
    ) -> Self {
        Self::Tagged(TaggedEntry::SessionMeta(SessionMeta {
            session_id: session_id.into(),
            cwd: cwd.into(),
            model,
            created_at: chrono::Utc::now(),
        }))
    }

    /// Classify a message into the entry it really is.
    ///
    /// A system message whose text carries [`COMPACT_BOUNDARY_MARKER`] is a
    /// compaction boundary, however it got into the message list; everything
    /// else stays a message.
    #[must_use]
    pub fn from_message(message: Message) -> Self {
        if is_compact_boundary_message(&message) {
            return Self::Tagged(TaggedEntry::CompactBoundary(CompactBoundary {
                at: chrono::Utc::now(),
                note: message_text(&message).unwrap_or_default().to_string(),
            }));
        }
        Self::Message(message)
    }

    /// The kind of entry this is.
    #[must_use]
    pub const fn kind(&self) -> EntryKind {
        match self {
            Self::Message(message) => match (&message.role, &message.content) {
                (Role::User, _) => EntryKind::UserText,
                (Role::Assistant, MessageContent::ToolCall(_)) => EntryKind::AssistantToolCall,
                (Role::Assistant, _) => EntryKind::AssistantText,
                (Role::Tool, _) => EntryKind::ToolResult,
                // A system message, and — because `Role` is `#[non_exhaustive]`
                // — a role added later, which reads as system text until it is
                // given a kind of its own.
                _ => EntryKind::SystemText,
            },
            Self::Tagged(TaggedEntry::CompactBoundary(_)) => EntryKind::CompactBoundary,
            Self::Tagged(TaggedEntry::SessionMeta(_)) => EntryKind::SessionMeta,
        }
    }

    /// Whether a person replaying the session would see this.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.kind().is_visible()
    }

    /// The message this entry stands for, if it stands for one.
    ///
    /// A compaction boundary becomes the system message the engine uses for
    /// it, so a reader that only knows about messages loses nothing. A session
    /// header is not a message and yields `None`.
    ///
    /// A boundary always yields a message that [`is_compact_boundary_message`]
    /// recognizes — a note that does not already carry the marker is wrapped in
    /// it. The type and the string therefore agree in both directions, which is
    /// what stops a boundary from quietly becoming ordinary system text.
    #[must_use]
    pub fn to_message(&self) -> Option<Message> {
        match self {
            Self::Message(message) => Some(message.clone()),
            Self::Tagged(TaggedEntry::CompactBoundary(boundary)) => {
                Some(Message::system(boundary_message_text(&boundary.note)))
            }
            Self::Tagged(TaggedEntry::SessionMeta(_)) => None,
        }
    }
}

/// The system-message text for a boundary note, marker included.
fn boundary_message_text(note: &str) -> String {
    if note.trim_start().starts_with(COMPACT_BOUNDARY_MARKER) {
        return note.to_string();
    }
    format!("{COMPACT_BOUNDARY_MARKER}\n{note}\n{COMPACT_BOUNDARY_CLOSE}")
}

/// Whether a message is the compaction boundary marker.
#[must_use]
pub fn is_compact_boundary_message(message: &Message) -> bool {
    matches!(message.role, Role::System)
        && message_text(message)
            .is_some_and(|text| text.trim_start().starts_with(COMPACT_BOUNDARY_MARKER))
}

/// The text of a message, when it carries any.
fn message_text(message: &Message) -> Option<&str> {
    match &message.content {
        MessageContent::Text(text) => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::ToolCall;

    /// Two entries are the same when the lines they serialize to are the same.
    fn same(a: &TranscriptEntry, b: &TranscriptEntry) -> bool {
        serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
    }

    #[test]
    fn a_plain_message_serializes_bare() {
        let entry = TranscriptEntry::message(Message::user("hello"));
        let line = serde_json::to_string(&entry).unwrap();
        // The bare form is what every existing transcript line looks like.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            serde_json::json!({"role": "User", "content": {"Text": "hello"}})
        );
    }

    #[test]
    fn an_old_bare_message_line_still_parses() {
        let line = r#"{"role":"Assistant","content":{"Text":"hi"}}"#;
        let entry: TranscriptEntry = serde_json::from_str(line).unwrap();
        assert_eq!(entry.kind(), EntryKind::AssistantText);
        assert_eq!(entry.to_message().unwrap().role, Role::Assistant);
    }

    #[test]
    fn a_boundary_is_classified_from_a_system_message() {
        let message = Message::system(format!("{COMPACT_BOUNDARY_MARKER}\nhistory removed"));
        let entry = TranscriptEntry::from_message(message);

        assert_eq!(entry.kind(), EntryKind::CompactBoundary);
        assert!(entry.is_visible());
        // And it turns back into the same system message.
        let restored = entry.to_message().unwrap();
        assert_eq!(restored.role, Role::System);
        assert!(is_compact_boundary_message(&restored));
    }

    #[test]
    fn a_typed_boundary_round_trips_through_json() {
        let entry = TranscriptEntry::compact_boundary("history removed here");
        let line = serde_json::to_string(&entry).unwrap();
        assert!(line.contains("\"type\":\"compact_boundary\""), "{line}");

        let decoded: TranscriptEntry = serde_json::from_str(&line).unwrap();
        assert!(same(&decoded, &entry));

        // A boundary always comes back as a message a marker check recognizes,
        // even when the note was written without the marker.
        let message = decoded.to_message().unwrap();
        assert!(is_compact_boundary_message(&message));
        assert!(
            message_text(&message)
                .unwrap()
                .contains("history removed here")
        );
    }

    #[test]
    fn a_boundary_note_that_already_carries_the_marker_is_not_wrapped_twice() {
        let original = Message::system(format!(
            "{COMPACT_BOUNDARY_MARKER}\nremoved\n{COMPACT_BOUNDARY_CLOSE}"
        ));
        let restored = TranscriptEntry::from_message(original.clone())
            .to_message()
            .unwrap();
        assert_eq!(message_text(&restored), message_text(&original));
    }

    #[test]
    fn a_session_header_is_not_a_message() {
        let entry = TranscriptEntry::session_meta("sess_1", "/tmp/project", Some("m".into()));
        assert_eq!(entry.kind(), EntryKind::SessionMeta);
        assert!(!entry.is_visible());
        assert!(entry.to_message().is_none());

        let line = serde_json::to_string(&entry).unwrap();
        assert!(line.contains("\"type\":\"session_meta\""), "{line}");
        let decoded: TranscriptEntry = serde_json::from_str(&line).unwrap();
        assert!(same(&decoded, &entry));
    }

    #[test]
    fn every_message_kind_is_named() {
        let cases = [
            (Message::user("u"), EntryKind::UserText),
            (Message::assistant("a"), EntryKind::AssistantText),
            (
                Message {
                    role: Role::Assistant,
                    content: MessageContent::ToolCall(ToolCall {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({}),
                    }),
                },
                EntryKind::AssistantToolCall,
            ),
            (Message::system("s"), EntryKind::SystemText),
        ];

        for (message, expected) in cases {
            let entry = TranscriptEntry::from_message(message);
            assert_eq!(entry.kind(), expected, "{entry:?}");
            assert!(entry.is_visible());
        }
    }
}
