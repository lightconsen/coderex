//! Session transcript resume helpers, interactive TUI snapshot persistence,
//! and the workspace-local SQLite session index.
//!
//! # Index wiring
//!
//! Transcripts remain the source of truth for `/resume`; the index is an
//! accelerator and a search surface. Callers outside this module (the REPL,
//! which this module does not edit) should wire it at two points:
//!
//! - **After persistence.** In `ReplLoop::persist_interactive_snapshot`
//!   (`crates/cli/src/repl.rs`), after `transcript_store.rewrite` and
//!   `save_snapshot`, call
//!   `index_saved_session(&self.cwd, &session_id,
//!   self.current_session_title().as_deref(), &session.messages)`.
//! - **From `/resume` search.** In `ReplLoop::resume_tui_session`
//!   (`crates/cli/src/repl.rs`) and the headless `/resume` arm, when the
//!   argument is not an exact session id, call
//!   `search_sessions(&self.cwd, query, 10)` (or
//!   `list_recent_sessions(&self.cwd, 10)` when no query was given) and
//!   let the user pick; load the chosen id through the existing transcript
//!   path.
//!
//! Every index operation is best-effort: a corrupt or locked database logs a
//! warning and the caller falls back to scanning
//! `<cwd>/.coderex/transcripts/*.jsonl`.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use coderex_config::dirs::DataDirectories;
use coderex_core::types::{Message, MessageContent, Role};
use coderex_memory::session_index::{SessionIndex, SessionMeta};
use coderex_memory::transcript::TranscriptStore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Persisted representation of a visible interactive plan step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedPlanStep {
    /// Short user-facing label.
    pub label: String,
    /// Whether the step is complete.
    pub complete: bool,
}

/// Persisted lightweight plan state for interactive resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PersistedInteractivePlan {
    /// Visible steps tracked for the most recent turn.
    pub steps: Vec<PersistedPlanStep>,
    /// Zero-based current step index.
    pub current_step: usize,
    /// Label for the active step.
    pub current_label: Option<String>,
}

/// Persisted interactive TUI state used to restore product feel on resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct InteractiveResumeSnapshot {
    /// Optional session title remembered at save time.
    pub session_title: Option<String>,
    /// Last visible plan state.
    pub plan: PersistedInteractivePlan,
    /// Recent concise activity rows.
    pub activity_log: Vec<String>,
}

impl InteractiveResumeSnapshot {
    /// Render a concise unfinished-task summary for resume messages.
    #[must_use]
    pub fn unfinished_context(&self) -> Option<String> {
        let mut sections = Vec::new();

        if let Some(label) = self
            .plan
            .current_label
            .as_ref()
            .filter(|label| !label.trim().is_empty())
        {
            let total = self.plan.steps.len();
            if total > 0 {
                sections.push(format!(
                    "Last known step: {}/{} — {}",
                    self.plan.current_step.saturating_add(1).min(total),
                    total,
                    label
                ));
            } else {
                sections.push(format!("Last known step: {label}"));
            }
        }

        if !self.activity_log.is_empty() {
            let recent = self
                .activity_log
                .iter()
                .rev()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>();
            sections.push(format!("Recent activity:\n{}", recent.join("\n")));
        }

        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }
}

/// Path to the transcript directory in the current workspace.
#[must_use]
pub fn transcript_dir(cwd: &str) -> PathBuf {
    Path::new(cwd).join(".coderex/transcripts")
}

/// Path to the persisted interactive snapshot for a session.
#[must_use]
pub fn snapshot_path(cwd: &str, session_id: &str) -> PathBuf {
    transcript_dir(cwd).join(format!("{session_id}.interactive.json"))
}

/// Path to the workspace-local SQLite session index.
///
/// `DataDirectories` owns the layout; rooted at `<cwd>/.coderex`, its
/// `sessions_db_path` resolves to `<cwd>/.coderex/sessions.db`.
#[must_use]
pub fn sessions_db_path(cwd: &str) -> PathBuf {
    DataDirectories::new(Path::new(cwd).join(".coderex")).sessions_db_path()
}

/// Largest transcript excerpt written into the index for one session.
const MAX_INDEXED_CONTENT_BYTES: usize = 256 * 1024;

/// Create or update the index row for a saved session.
///
/// Call this after the transcript and interactive snapshot are persisted.
/// The write is atomic and idempotent: re-indexing the same `session_id`
/// updates the existing row instead of adding a duplicate. A corrupt or
/// locked database is reported as a warning and never fails the caller —
/// the transcript is still the source of truth for `/resume`.
pub fn index_saved_session(cwd: &str, session_id: &str, title: Option<&str>, messages: &[Message]) {
    if let Err(error) = try_index_saved_session(cwd, session_id, title, messages) {
        tracing::warn!(
            session_id,
            db = %sessions_db_path(cwd).display(),
            "session index unavailable; transcripts remain authoritative: {error}"
        );
    }
}

fn try_index_saved_session(
    cwd: &str,
    session_id: &str,
    title: Option<&str>,
    messages: &[Message],
) -> Result<()> {
    let index = SessionIndex::open(&sessions_db_path(cwd))?;
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
    let content = messages_content(messages);
    index.upsert_with_content(&meta, Some(&content))?;
    Ok(())
}

/// Search indexed sessions, newest first.
///
/// The SQLite FTS index is consulted first. Sessions it does not know about
/// (older transcripts, or a corrupt/locked database) are found by scanning
/// the workspace transcripts for the query as a case-insensitive substring.
/// Results are de-duplicated by session id. An empty query lists recent
/// sessions.
#[must_use]
pub fn search_sessions(cwd: &str, query: &str, limit: usize) -> Vec<SessionMeta> {
    if limit == 0 {
        return Vec::new();
    }
    let query = query.trim();
    if query.is_empty() {
        return list_recent_sessions(cwd, limit);
    }

    let fts = fts_query(query);
    let mut results = if fts.is_empty() {
        Vec::new()
    } else {
        match SessionIndex::open(&sessions_db_path(cwd)) {
            Ok(index) => match index.search(&fts, limit) {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(
                        db = %sessions_db_path(cwd).display(),
                        "session index search failed; falling back to transcripts: {error}"
                    );
                    Vec::new()
                }
            },
            Err(error) => {
                tracing::warn!(
                    db = %sessions_db_path(cwd).display(),
                    "session index unavailable; falling back to transcripts: {error}"
                );
                Vec::new()
            }
        }
    };

    for meta in search_transcripts(cwd, query) {
        if !results
            .iter()
            .any(|existing| existing.session_id == meta.session_id)
        {
            results.push(meta);
        }
    }

    results.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    results.truncate(limit);
    results
}

/// List recent sessions, newest first.
///
/// Indexed sessions are merged with any transcript-only sessions, so the
/// result is useful both before the index has been populated and after.
#[must_use]
pub fn list_recent_sessions(cwd: &str, limit: usize) -> Vec<SessionMeta> {
    if limit == 0 {
        return Vec::new();
    }

    let mut sessions = match SessionIndex::open(&sessions_db_path(cwd)) {
        Ok(index) => match index.list_by_project(cwd) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    db = %sessions_db_path(cwd).display(),
                    "session index list failed; falling back to transcripts: {error}"
                );
                Vec::new()
            }
        },
        Err(error) => {
            tracing::warn!(
                db = %sessions_db_path(cwd).display(),
                "session index unavailable; falling back to transcripts: {error}"
            );
            Vec::new()
        }
    };

    for meta in transcript_metas(cwd) {
        if !sessions
            .iter()
            .any(|existing| existing.session_id == meta.session_id)
        {
            sessions.push(meta);
        }
    }

    sessions.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    sessions.truncate(limit);
    sessions
}

/// Turn a free-form query into safe FTS5 terms.
///
/// `SessionIndex::search` appends the prefix `*` itself, so adding one here
/// would produce `term**` and an FTS syntax error.
fn fts_query(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Flatten transcript messages into the text stored in the FTS entry.
fn messages_content(messages: &[Message]) -> String {
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

/// Every JSONL transcript in the workspace, newest first.
fn transcript_files(cwd: &str) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(transcript_dir(cwd)) else {
        return Vec::new();
    };
    let mut files = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                return None;
            }
            let modified = entry.metadata().and_then(|meta| meta.modified()).ok()?;
            Some((path, modified))
        })
        .collect::<Vec<_>>();
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files
}

fn transcript_metas(cwd: &str) -> Vec<SessionMeta> {
    transcript_files(cwd)
        .into_iter()
        .filter_map(|(path, modified)| meta_from_transcript(cwd, &path, modified))
        .collect()
}

fn search_transcripts(cwd: &str, query: &str) -> Vec<SessionMeta> {
    let needle = query.to_lowercase();
    transcript_files(cwd)
        .into_iter()
        .filter_map(|(path, modified)| {
            let bytes = std::fs::read(&path).ok()?;
            let raw = String::from_utf8_lossy(&bytes);
            if raw.to_lowercase().contains(&needle) {
                meta_from_transcript(cwd, &path, modified)
            } else {
                None
            }
        })
        .collect()
}

fn meta_from_transcript(cwd: &str, path: &Path, modified: SystemTime) -> Option<SessionMeta> {
    let session_id = path.file_stem().and_then(|stem| stem.to_str())?.to_string();
    let bytes = std::fs::read(path).ok()?;
    let raw = String::from_utf8_lossy(&bytes);
    let last_active_at: DateTime<Utc> = modified.into();
    Some(SessionMeta {
        session_id,
        project_path: cwd.to_string(),
        ai_title: transcript_title(&raw),
        tag: None,
        pr_url: None,
        created_at: last_active_at,
        last_active_at,
        total_cost_usd: 0.0,
        total_tokens: 0,
    })
}

fn transcript_title(raw: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let message: Message = serde_json::from_str(line).ok()?;
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

/// Load transcript messages for a resumed session.
pub fn load_resume_messages(cwd: &str, session_id: &str) -> Result<Vec<Message>> {
    let transcript_dir = transcript_dir(cwd);
    let store = TranscriptStore::new(transcript_dir.clone());
    let messages = store.read_all_skip_first_prompt(session_id)?;

    if messages.is_empty() {
        bail!(
            "No transcript found for session '{session_id}' in {}",
            transcript_dir.display()
        );
    }

    Ok(messages)
}

/// Find the latest session id from transcript files in the current workspace.
pub fn latest_session_id(cwd: &str) -> Option<String> {
    let transcript_dir = transcript_dir(cwd);
    if !transcript_dir.exists() {
        return None;
    }

    let mut latest: Option<(std::time::SystemTime, String)> = None;
    let entries = std::fs::read_dir(&transcript_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(session_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
        else {
            continue;
        };
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if latest.as_ref().is_none_or(|(last, _)| modified > *last) {
            latest = Some((modified, session_id));
        }
    }

    latest.map(|(_, session_id)| session_id)
}

/// Persist the latest interactive UI snapshot for a session.
pub fn save_snapshot(
    cwd: &str,
    session_id: &str,
    snapshot: &InteractiveResumeSnapshot,
) -> std::io::Result<()> {
    let path = snapshot_path(cwd, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(snapshot)?;
    std::fs::write(path, content)
}

/// Load a persisted interactive UI snapshot, if present.
pub fn load_snapshot(cwd: &str, session_id: &str) -> Option<InteractiveResumeSnapshot> {
    let content = std::fs::read_to_string(snapshot_path(cwd, session_id)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Remove a persisted interactive UI snapshot.
pub fn remove_snapshot(cwd: &str, session_id: &str) -> std::io::Result<()> {
    let path = snapshot_path(cwd, session_id);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_session_id_uses_jsonl_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let dir = transcript_dir(&cwd);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("older.jsonl"), "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("newer.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("ignore.interactive.json"), "{}\n").unwrap();

        assert_eq!(latest_session_id(&cwd).as_deref(), Some("newer"));
    }

    #[test]
    fn save_and_load_snapshot_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let snapshot = InteractiveResumeSnapshot {
            session_title: Some("Resume me".into()),
            plan: PersistedInteractivePlan {
                steps: vec![
                    PersistedPlanStep {
                        label: "Planning".into(),
                        complete: true,
                    },
                    PersistedPlanStep {
                        label: "Running tests".into(),
                        complete: false,
                    },
                ],
                current_step: 1,
                current_label: Some("Running tests".into()),
            },
            activity_log: vec!["Planning".into(), "Running tests".into()],
        };

        save_snapshot(&cwd, "sess_1", &snapshot).unwrap();
        assert_eq!(load_snapshot(&cwd, "sess_1"), Some(snapshot));
    }

    #[test]
    fn unfinished_context_summarizes_plan_and_activity() {
        let snapshot = InteractiveResumeSnapshot {
            session_title: None,
            plan: PersistedInteractivePlan {
                steps: vec![PersistedPlanStep {
                    label: "Running tests".into(),
                    complete: false,
                }],
                current_step: 0,
                current_label: Some("Running tests".into()),
            },
            activity_log: vec![
                "Planning".into(),
                "Reading Cargo.toml".into(),
                "Running cargo test".into(),
            ],
        };

        let summary = snapshot.unfinished_context().unwrap();
        assert!(summary.contains("Last known step"));
        assert!(summary.contains("Running cargo test"));
    }

    #[test]
    fn index_saved_session_roundtrips_through_search_and_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let messages = vec![
            Message::user("Investigate the authentication regression"),
            Message::assistant("Found it in the oauth2 token refresh"),
        ];

        index_saved_session(&cwd, "sess_auth", Some("Authentication fix"), &messages);

        assert!(sessions_db_path(&cwd).exists());

        let hits = search_sessions(&cwd, "auth", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess_auth");
        assert_eq!(hits[0].ai_title.as_deref(), Some("Authentication fix"));

        // Transcript content, not just the title, is searchable.
        let content_hits = search_sessions(&cwd, "oauth2", 10);
        assert_eq!(content_hits.len(), 1);
        assert_eq!(content_hits[0].session_id, "sess_auth");

        let recent = list_recent_sessions(&cwd, 10);
        assert!(recent.iter().any(|meta| meta.session_id == "sess_auth"));

        let index = SessionIndex::open(&sessions_db_path(&cwd)).unwrap();
        let stored = index.get("sess_auth").unwrap().unwrap();
        assert_eq!(stored.project_path, cwd);
    }

    #[test]
    fn corrupt_index_falls_back_to_transcripts_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        let store = TranscriptStore::new(transcript_dir(&cwd));
        store
            .append("sess_old", &Message::user("the widget migration failed"))
            .unwrap();

        let db_path = sessions_db_path(&cwd);
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(&db_path, b"not a sqlite database").unwrap();

        // Indexing tolerates the corrupt database: it warns, and does not panic.
        index_saved_session(&cwd, "sess_new", Some("New"), &[Message::user("hello")]);

        let hits = search_sessions(&cwd, "widget", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess_old");
        assert!(
            hits[0]
                .ai_title
                .as_deref()
                .unwrap_or_default()
                .contains("widget")
        );

        let recent = list_recent_sessions(&cwd, 10);
        assert!(recent.iter().any(|meta| meta.session_id == "sess_old"));
    }

    #[test]
    fn reindexing_the_same_session_updates_without_duplicating() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        index_saved_session(
            &cwd,
            "sess_dup",
            Some("First title"),
            &[Message::user("alpha")],
        );
        index_saved_session(
            &cwd,
            "sess_dup",
            Some("Second title"),
            &[Message::user("beta")],
        );

        let index = SessionIndex::open(&sessions_db_path(&cwd)).unwrap();
        let stored = index.get("sess_dup").unwrap().unwrap();
        assert_eq!(stored.ai_title.as_deref(), Some("Second title"));
        assert_eq!(index.list_by_project(&cwd).unwrap().len(), 1);
        assert_eq!(list_recent_sessions(&cwd, 10).len(), 1);

        let updated = search_sessions(&cwd, "Second", 10);
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].session_id, "sess_dup");

        // The old FTS entry was replaced, not left behind.
        assert!(search_sessions(&cwd, "First", 10).is_empty());
    }
}
