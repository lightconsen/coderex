//! The journal — how a workflow resumes instead of starting over.
//!
//! Every `agent()` call gets a key derived from the call itself and from the
//! key of the call before it. That chaining is the whole mechanism: change one
//! call and its key changes, and so does every key after it, so a resume
//! re-runs the changed call and everything downstream while replaying the rest
//! from the journal. It is the longest unchanged prefix, expressed as a hash
//! chain rather than as a diff.
//!
//! The journal is a line-per-entry file, appended as agents finish. A run that
//! is killed leaves a half-written last line, which is skipped rather than
//! treated as corruption — the entry was never completed, so it never
//! happened.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The prefix every key carries, so a key from an older scheme is obvious.
pub const KEY_PREFIX: &str = "v2:";

/// The file a run's journal lives in.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// A key with nothing before it.
pub const ROOT_KEY: &str = "genesis";

/// What the journal says about a run that was interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreState {
    /// No journal at all.
    Missing,
    /// Entries were written, but the run did not finish writing them back.
    Partial,
    /// The run wrote its entries back and marked itself restored.
    Complete,
}

/// One line of the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// An agent call started.
    Started {
        /// The call's key.
        key: String,
        /// The agent it started.
        agent_id: String,
        /// The label, if the script gave one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The phase, if there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    /// An agent call finished.
    Result {
        /// The call's key.
        key: String,
        /// The agent it was.
        agent_id: String,
        /// What it returned, or `null` if it produced nothing.
        result: Option<String>,
    },
    /// An agent call failed.
    Failed {
        /// The call's key.
        key: String,
        /// The agent it was.
        agent_id: String,
    },
    /// A resume began rewriting the journal.
    Restoring,
    /// The rewrite finished.
    Restored,
}

impl Entry {
    /// The key this entry is about, if it is about one.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Started { key, .. } | Self::Result { key, .. } | Self::Failed { key, .. } => {
                Some(key)
            }
            Self::Restoring | Self::Restored => None,
        }
    }
}

/// What a loaded journal holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    /// Results by key.
    pub results: BTreeMap<String, Option<String>>,
    /// Calls that started, by key.
    pub started: BTreeMap<String, String>,
    /// Calls that failed, by key.
    pub failed: Vec<String>,
    /// Lines that could not be read.
    pub unreadable: usize,
}

/// A run's journal on disk.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Open the journal for a run directory.
    #[must_use]
    pub fn open(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            path: run_dir.into().join(JOURNAL_FILE),
        }
    }

    /// The file entries are written to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry.
    ///
    /// # Errors
    ///
    /// Fails if the run directory or the file cannot be written.
    pub fn append(&self, entry: &Entry) -> Result<(), std::io::Error> {
        use std::io::Write as _;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")
    }

    /// Read the entries back.
    ///
    /// # Errors
    ///
    /// Fails if the file exists but cannot be read.
    pub fn load(&self) -> Result<Loaded, std::io::Error> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Loaded::default());
            }
            Err(error) => return Err(error),
        };

        let mut loaded = Loaded::default();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(line) {
                Ok(Entry::Result { key, result, .. }) => {
                    loaded.results.insert(key, result);
                }
                Ok(Entry::Started { key, agent_id, .. }) => {
                    loaded.started.insert(key, agent_id);
                }
                Ok(Entry::Failed { key, .. }) => loaded.failed.push(key),
                Ok(Entry::Restoring | Entry::Restored) => {}
                // A killed process leaves a half-written line; the entry it
                // was going to be never happened.
                Err(_) => loaded.unreadable += 1,
            }
        }
        Ok(loaded)
    }

    /// What state the journal is in.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read.
    pub fn restore_state(&self) -> Result<RestoreState, std::io::Error> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RestoreState::Missing);
            }
            Err(error) => return Err(error),
        };

        let mut first = None;
        let mut restored = false;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(line) {
                Ok(Entry::Restoring) if first.is_none() => first = Some(Entry::Restoring),
                Ok(Entry::Restored) => restored = true,
                Ok(_) | Err(_) => {}
            }
        }

        Ok(match (first, restored) {
            (Some(_), false) => RestoreState::Partial,
            _ => RestoreState::Complete,
        })
    }

    /// Rewrite the journal with the entries a resume kept.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be rewritten.
    pub fn write_restored(&self, entries: &[Entry]) -> Result<(), std::io::Error> {
        let mut text = String::new();
        text.push_str(&serde_json::to_string(&Entry::Restoring).map_err(std::io::Error::other)?);
        text.push('\n');
        for entry in entries {
            text.push_str(&serde_json::to_string(entry).map_err(std::io::Error::other)?);
            text.push('\n');
        }
        text.push_str(&serde_json::to_string(&Entry::Restored).map_err(std::io::Error::other)?);
        text.push('\n');

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, text)
    }
}

/// The key for an agent call.
///
/// `previous` is the key of the call before it, which is what makes the chain:
/// the same call at the same point gets the same key, and a change anywhere
/// before it changes every key after it.
#[must_use]
pub fn key_for(previous: &str, prompt: &str, options: &CallOptions) -> String {
    let mut hasher = Sha256::new();
    hasher.update(previous.as_bytes());
    hasher.update(b"\0");
    hasher.update(prompt.as_bytes());
    hasher.update(b"\0");
    hasher.update(options.canonical().as_bytes());
    format!("{KEY_PREFIX}{}", hex(&hasher.finalize()))
}

/// The options that are part of a call's identity.
///
/// Only the ones that *change what the agent is asked to do* are included: a
/// label is for the person watching, so renaming an agent must not invalidate
/// everything after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallOptions {
    /// A schema the answer must match.
    pub schema: Option<String>,
    /// The model to use.
    pub model: Option<String>,
    /// The effort level.
    pub effort: Option<String>,
    /// Whether the agent runs in its own worktree.
    pub isolation: Option<String>,
    /// The kind of agent.
    pub agent_type: Option<String>,
    /// Tools the agent may not use.
    pub disallowed_tools: Vec<String>,
    /// A permission clamp on shell commands.
    pub bash_clamp: Option<String>,
}

impl CallOptions {
    /// The options as a stable string, with the keys in a fixed order.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("schema={}", self.schema.as_deref().unwrap_or("")));
        parts.push(format!("model={}", self.model.as_deref().unwrap_or("")));
        parts.push(format!("effort={}", self.effort.as_deref().unwrap_or("")));
        parts.push(format!(
            "isolation={}",
            self.isolation.as_deref().unwrap_or("")
        ));
        parts.push(format!(
            "agentType={}",
            self.agent_type.as_deref().unwrap_or("")
        ));
        let mut tools = self.disallowed_tools.clone();
        tools.sort();
        parts.push(format!("disallowedTools={}", tools.join(",")));
        parts.push(format!(
            "bashCommandClamp={}",
            self.bash_clamp.as_deref().unwrap_or("")
        ));
        parts.join("\n")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(model: &str) -> CallOptions {
        CallOptions {
            model: Some(model.to_string()),
            ..CallOptions::default()
        }
    }

    #[test]
    fn the_same_call_gets_the_same_key() {
        let first = key_for(ROOT_KEY, "review this", &options("sonnet"));
        let again = key_for(ROOT_KEY, "review this", &options("sonnet"));
        assert_eq!(first, again);
        assert!(first.starts_with(KEY_PREFIX));
    }

    #[test]
    fn changing_a_call_changes_its_key_and_everything_after_it() {
        let a = key_for(ROOT_KEY, "first", &CallOptions::default());
        let b = key_for(&a, "second", &CallOptions::default());

        // The same second call after a changed first one is a different call.
        let changed_a = key_for(ROOT_KEY, "first, with a tweak", &CallOptions::default());
        let changed_b = key_for(&changed_a, "second", &CallOptions::default());
        assert_ne!(a, changed_a);
        assert_ne!(b, changed_b);

        // … while everything before the change still matches, which is what
        // makes a resume replay the unchanged prefix.
        assert_eq!(a, key_for(ROOT_KEY, "first", &CallOptions::default()));
    }

    #[test]
    fn options_that_change_the_work_change_the_key() {
        let base = key_for(ROOT_KEY, "p", &CallOptions::default());

        assert_ne!(base, key_for(ROOT_KEY, "p", &options("opus")));
        assert_ne!(
            base,
            key_for(
                ROOT_KEY,
                "p",
                &CallOptions {
                    schema: Some("{\"type\":\"object\"}".into()),
                    ..CallOptions::default()
                }
            )
        );
        assert_ne!(
            base,
            key_for(
                ROOT_KEY,
                "p",
                &CallOptions {
                    isolation: Some("worktree".into()),
                    ..CallOptions::default()
                }
            )
        );
        assert_ne!(
            base,
            key_for(
                ROOT_KEY,
                "p",
                &CallOptions {
                    disallowed_tools: vec!["Bash".into()],
                    ..CallOptions::default()
                }
            )
        );
    }

    #[test]
    fn the_order_of_disallowed_tools_does_not_change_the_key() {
        let one = CallOptions {
            disallowed_tools: vec!["Bash".into(), "Edit".into()],
            ..CallOptions::default()
        };
        let other = CallOptions {
            disallowed_tools: vec!["Edit".into(), "Bash".into()],
            ..CallOptions::default()
        };
        assert_eq!(key_for(ROOT_KEY, "p", &one), key_for(ROOT_KEY, "p", &other));
    }

    #[test]
    fn entries_round_trip_through_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path());

        journal
            .append(&Entry::Started {
                key: "v2:aa".into(),
                agent_id: "agent-1".into(),
                label: Some("first".into()),
                phase: Some("Review".into()),
            })
            .unwrap();
        journal
            .append(&Entry::Result {
                key: "v2:aa".into(),
                agent_id: "agent-1".into(),
                result: Some("all clear".into()),
            })
            .unwrap();
        journal
            .append(&Entry::Failed {
                key: "v2:bb".into(),
                agent_id: "agent-2".into(),
            })
            .unwrap();

        let loaded = journal.load().unwrap();
        assert_eq!(loaded.results["v2:aa"], Some("all clear".to_string()));
        assert_eq!(loaded.started["v2:aa"], "agent-1");
        assert_eq!(loaded.failed, vec!["v2:bb".to_string()]);
        assert_eq!(loaded.unreadable, 0);
    }

    #[test]
    fn a_result_of_nothing_is_still_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path());
        journal
            .append(&Entry::Result {
                key: "v2:aa".into(),
                agent_id: "a".into(),
                result: None,
            })
            .unwrap();

        // A skipped agent returns null; the journal has to remember that it
        // was skipped rather than treat it as never having run.
        let loaded = journal.load().unwrap();
        assert!(loaded.results.contains_key("v2:aa"));
        assert_eq!(loaded.results["v2:aa"], None);
    }

    #[test]
    fn a_half_written_last_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path());
        journal
            .append(&Entry::Result {
                key: "v2:aa".into(),
                agent_id: "a".into(),
                result: Some("ok".into()),
            })
            .unwrap();

        let mut text = std::fs::read_to_string(journal.path()).unwrap();
        text.push_str("{\"type\":\"resu");
        std::fs::write(journal.path(), text).unwrap();

        let loaded = journal.load().unwrap();
        assert_eq!(loaded.results.len(), 1);
        assert_eq!(loaded.unreadable, 1);
    }

    #[test]
    fn a_journal_that_is_not_there_reads_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("run-1"));

        assert_eq!(journal.restore_state().unwrap(), RestoreState::Missing);
        assert_eq!(journal.load().unwrap(), Loaded::default());
    }

    #[test]
    fn a_run_that_restored_itself_reads_as_complete() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path());

        journal
            .append(&Entry::Result {
                key: "v2:aa".into(),
                agent_id: "a".into(),
                result: Some("ok".into()),
            })
            .unwrap();
        assert_eq!(journal.restore_state().unwrap(), RestoreState::Complete);

        journal.write_restored(&[]).unwrap();
        assert_eq!(journal.restore_state().unwrap(), RestoreState::Complete);
    }

    #[test]
    fn a_run_that_was_interrupted_mid_rewrite_reads_as_partial() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path());

        // `write_restored` writes the marker first; a process killed between
        // the two lines leaves exactly this.
        let text = format!(
            "{}\n{}\n",
            serde_json::to_string(&Entry::Restoring).unwrap(),
            serde_json::to_string(&Entry::Result {
                key: "v2:aa".into(),
                agent_id: "a".into(),
                result: Some("ok".into()),
            })
            .unwrap()
        );
        std::fs::write(journal.path(), text).unwrap();

        assert_eq!(journal.restore_state().unwrap(), RestoreState::Partial);
    }

    #[test]
    fn entries_survive_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path());
        let kept = vec![Entry::Result {
            key: "v2:aa".into(),
            agent_id: "a".into(),
            result: Some("kept".into()),
        }];

        journal.write_restored(&kept).unwrap();
        journal
            .append(&Entry::Result {
                key: "v2:bb".into(),
                agent_id: "b".into(),
                result: Some("new".into()),
            })
            .unwrap();

        let loaded = journal.load().unwrap();
        assert_eq!(loaded.results["v2:aa"], Some("kept".to_string()));
        assert_eq!(loaded.results["v2:bb"], Some("new".to_string()));
        assert_eq!(journal.restore_state().unwrap(), RestoreState::Complete);
    }
}
