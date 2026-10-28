//! Backfilling the session index from transcripts that already exist.
//!
//! The index is a search accelerator written as a side effect of saving a
//! session. A workspace that predates the index — or one whose database was
//! deleted — has transcripts and no rows, so `/resume` search falls back to
//! scanning files. [`backfill`] walks the workspace's transcripts and writes
//! the rows the save path would have written.
//!
//! It lives here rather than in the CLI because the slash command that runs it
//! (`crates/commands`) cannot depend on `coderex-cli`; the command crate does
//! depend on this one. The CLI's save path uses the same [`session_content`]
//! and [`index_transcript`] helpers, so a backfilled row and a saved row are
//! indexed identically.
//!
//! Ordering matters for the FTS table: it is an external-content-free FTS5
//! table, so documents are deleted before they are re-inserted. Re-indexing an
//! existing session therefore replaces its entry instead of appending a second
//! copy — the same contract [`SessionIndex::upsert_with_content`] keeps for the
//! metadata row.

use chrono::{DateTime, Utc};
use coderex_core::types::{Message, MessageContent, Role};
use std::path::{Path, PathBuf};

use crate::entry::TranscriptEntry;
use crate::session_index::{SessionIndex, SessionIndexError, SessionMeta};
use crate::transcript::TranscriptStore;

/// Largest transcript excerpt written into the index for one session.
pub const MAX_INDEXED_CONTENT_BYTES: usize = 256 * 1024;

/// What a backfill did, so a caller can report it without re-scanning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// Transcript files found.
    pub transcripts: usize,
    /// Files whose index row was written or refreshed.
    pub indexed: usize,
    /// Files skipped because the transcript could not be read or parsed.
    pub skipped: usize,
}

impl BackfillReport {
    /// Whether anything was written.
    #[must_use]
    pub const fn wrote_anything(&self) -> bool {
        self.indexed > 0
    }
}

/// Index every transcript in `cwd`'s session transcript directory.
///
/// `session_transcripts` is `<cwd>/.coderex/transcripts`; the index lives at
/// the path the caller opened, which the CLI derives from the same `.coderex`
/// base directory.
///
/// A transcript that cannot be read is counted as skipped rather than failing
/// the whole run: one corrupt file must not block the other sessions from
/// being searchable.
///
/// # Errors
///
/// [`SessionIndexError`] when the index itself refuses a write (a closed or
/// corrupt database). A per-file problem is reported in the returned
/// [`BackfillReport`] instead.
pub fn backfill(
    index: &SessionIndex,
    cwd: &str,
    session_transcripts: &Path,
) -> Result<BackfillReport, SessionIndexError> {
    let store = TranscriptStore::new(session_transcripts.to_path_buf());
    let mut report = BackfillReport::default();
    let Ok(entries) = std::fs::read_dir(session_transcripts) else {
        return Ok(report);
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        report.transcripts += 1;
        let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            report.skipped += 1;
            continue;
        };
        let Ok(raw) = std::fs::read(&path) else {
            report.skipped += 1;
            continue;
        };
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .map_or_else(|_| Utc::now(), DateTime::<Utc>::from);
        let title = transcript_title(&raw);
        let content = transcript_content(&store, session_id, &raw);
        let meta = SessionMeta {
            session_id: session_id.to_string(),
            project_path: cwd.to_string(),
            ai_title: title,
            tag: None,
            pr_url: None,
            created_at: modified,
            last_active_at: modified,
            total_cost_usd: 0.0,
            total_tokens: 0,
        };
        index.upsert_with_content(&meta, Some(&content))?;
        if !content.is_empty() {
            index.index_content(session_id, &content)?;
        }
        report.indexed += 1;
    }
    Ok(report)
}

/// Flatten a conversation into the text stored in the FTS entry.
///
/// Bounded by [`MAX_INDEXED_CONTENT_BYTES`] and truncated on a character
/// boundary, so a session with one enormous tool result does not turn into a
/// huge database row.
#[must_use]
pub fn session_content(messages: &[Message]) -> String {
    let mut content = String::new();
    for message in messages {
        match &message.content {
            MessageContent::Text(text) => content.push_str(text),
            MessageContent::ToolCall(call) => {
                content.push_str(&call.name);
                content.push(' ');
                content.push_str(&call.input.to_string());
            }
            MessageContent::ToolResult(result) => content.push_str(&result.output),
            // `MessageContent` is `#[non_exhaustive]`: a future variant simply
            // does not contribute text to the index.
            _ => {}
        }
        content.push('\n');
        if content.len() >= MAX_INDEXED_CONTENT_BYTES {
            break;
        }
    }
    truncate_at_char_boundary(content, MAX_INDEXED_CONTENT_BYTES)
}

/// Write (or refresh) the index row for a parsed conversation.
///
/// The save path and the backfill both land here, so the two cannot disagree
/// about what "indexed" means.
///
/// # Errors
///
/// [`SessionIndexError`] from the metadata row or the TF-IDF table.
pub fn index_transcript(
    index: &SessionIndex,
    cwd: &str,
    session_id: &str,
    title: Option<&str>,
    messages: &[Message],
) -> Result<(), SessionIndexError> {
    let now = Utc::now();
    let title = title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(ToString::to_string);
    let meta = SessionMeta {
        session_id: session_id.to_string(),
        project_path: cwd.to_string(),
        ai_title: title,
        tag: None,
        pr_url: None,
        created_at: now,
        last_active_at: now,
        total_cost_usd: 0.0,
        total_tokens: 0,
    };
    let content = session_content(messages);
    index.upsert_with_content(&meta, Some(&content))?;
    if !content.is_empty() {
        index.index_content(session_id, &content)?;
    }
    Ok(())
}

/// The text a transcript file contributes to its index entry.
///
/// The store's reader is preferred because it skips tagged lines the way the
/// rest of the codebase does; a file it refuses (an encrypted or truncated
/// transcript) falls back to the raw text, which is still searchable.
fn transcript_content(store: &TranscriptStore, session_id: &str, raw: &str) -> String {
    store.read_all(session_id).map_or_else(
        |_| truncate_at_char_boundary(raw.to_string(), MAX_INDEXED_CONTENT_BYTES),
        |messages| session_content(&messages),
    )
}

/// The first user prompt in a transcript, as a title.
///
/// The save path stores the model's generated title; a transcript that was
/// never saved through the CLI has no title, and the prompt is the best
/// human-readable stand-in.
#[must_use]
pub fn transcript_title(raw: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let entry: TranscriptEntry = serde_json::from_str(line).ok()?;
        let message = entry.to_message()?;
        if !matches!(message.role, Role::User) {
            return None;
        }
        let MessageContent::Text(text) = message.content else {
            return None;
        };
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            None
        } else {
            Some(truncate_title(&collapsed))
        }
    })
}

fn truncate_title(title: &str) -> String {
    const MAX_CHARS: usize = 80;
    if title.chars().count() <= MAX_CHARS {
        return title.to_string();
    }
    let mut truncated = title.chars().take(MAX_CHARS).collect::<String>();
    truncated.push('…');
    truncated
}

fn truncate_at_char_boundary(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// The transcript directory a workspace keeps its sessions in.
#[must_use]
pub fn transcript_dir(cwd: &str) -> PathBuf {
    Path::new(cwd).join(".coderex").join("transcripts")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(dir: &Path, session_id: &str, messages: &[Message]) {
        let store = TranscriptStore::new(dir.to_path_buf());
        store.rewrite(session_id, messages).unwrap();
    }

    #[test]
    fn backfill_indexes_every_transcript_once() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let transcripts = transcript_dir(&cwd);
        std::fs::create_dir_all(&transcripts).unwrap();
        write_transcript(
            &transcripts,
            "sess_a",
            &[
                Message::user("deploy the pipeline"),
                Message::assistant("started"),
            ],
        );
        write_transcript(
            &transcripts,
            "sess_b",
            &[Message::user("roll back the release")],
        );

        let index = SessionIndex::open(&tmp.path().join("sessions.db")).unwrap();
        let report = backfill(&index, &cwd, &transcripts).unwrap();
        assert_eq!(report.transcripts, 2);
        assert_eq!(report.indexed, 2);
        assert_eq!(report.skipped, 0);
        assert!(report.wrote_anything());

        let rows = index.list_by_project(&cwd).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| row.ai_title.as_deref() == Some("deploy the pipeline")),
            "the first user prompt did not become the title: {rows:?}"
        );
        // The content is searchable, not just the title.
        let hits = index.search("pipeline*", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    #[test]
    fn running_backfill_twice_does_not_duplicate_rows_or_search_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let transcripts = transcript_dir(&cwd);
        std::fs::create_dir_all(&transcripts).unwrap();
        write_transcript(&transcripts, "sess_once", &[Message::user("unique needle")]);

        let index = SessionIndex::open(&tmp.path().join("sessions.db")).unwrap();
        backfill(&index, &cwd, &transcripts).unwrap();
        backfill(&index, &cwd, &transcripts).unwrap();

        assert_eq!(index.list_by_project(&cwd).unwrap().len(), 1);
        assert_eq!(
            index.search("needle*", 10).unwrap().len(),
            1,
            "the FTS entry was appended rather than replaced"
        );
    }

    #[test]
    fn an_unreadable_transcript_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let transcripts = transcript_dir(&cwd);
        std::fs::create_dir_all(&transcripts).unwrap();
        write_transcript(&transcripts, "sess_ok", &[Message::user("good session")]);
        std::fs::write(transcripts.join("sess_bad.jsonl"), b"\xff\xfe not json").unwrap();

        let index = SessionIndex::open(&tmp.path().join("sessions.db")).unwrap();
        let report = backfill(&index, &cwd, &transcripts).unwrap();
        assert_eq!(report.transcripts, 2);
        assert_eq!(report.indexed, 2, "raw text still indexes");
        assert!(index.get("sess_ok").unwrap().is_some());
    }

    #[test]
    fn a_missing_transcript_directory_is_an_empty_report() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SessionIndex::open(&tmp.path().join("sessions.db")).unwrap();
        let report = backfill(&index, "/nowhere", &tmp.path().join("absent")).unwrap();
        assert_eq!(report, BackfillReport::default());
        assert!(!report.wrote_anything());
    }

    #[test]
    fn session_content_covers_text_calls_and_results() {
        let messages = vec![
            Message::user("look at src/lib.rs"),
            Message::assistant("reading"),
        ];
        let text = session_content(&messages);
        assert!(text.contains("look at src/lib.rs"), "{text}");
        assert!(text.contains("reading"), "{text}");
    }
}
