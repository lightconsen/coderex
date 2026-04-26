//! Session transcript resume helpers and interactive TUI snapshot persistence.

use anyhow::{Result, bail};
use coderex_core::types::Message;
use coderex_memory::transcript::TranscriptStore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// Load transcript messages for a resumed session.
pub fn load_resume_messages(cwd: &str, session_id: &str) -> Result<Vec<Message>> {
    let transcript_dir = transcript_dir(cwd);
    let store = TranscriptStore::new(transcript_dir.clone());
    let messages = store.read_all_skip_first_prompt(session_id)?;

    if messages.is_empty() {
        bail!("No transcript found for session '{session_id}' in {transcript_dir:?}");
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
}
