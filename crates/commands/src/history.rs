//! Command history — persistence and `/history` command.

use std::path::PathBuf;

/// A single command history entry.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    /// Command name (without leading /).
    pub cmd: String,
    /// Arguments passed.
    pub args: Vec<String>,
    /// Unix timestamp when the command was executed.
    pub timestamp: u64,
}

/// Command history with ring buffer and file persistence.
#[derive(Debug, Default)]
pub struct CommandHistory {
    entries: Vec<HistoryEntry>,
    max_entries: usize,
    file_path: Option<PathBuf>,
}

impl CommandHistory {
    /// Create a new command history with the given max entries.
    #[must_use]
    pub const fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_entries,
            file_path: None,
        }
    }

    /// Set the file path for persistence.
    pub fn set_file_path(&mut self, path: PathBuf) {
        self.file_path = Some(path);
    }

    /// Append a command to history.
    pub fn append(&mut self, cmd: &str, args: &[String]) {
        let entry = HistoryEntry {
            cmd: cmd.to_string(),
            args: args.to_vec(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        self.entries.push(entry);
        if self.entries.len() > self.max_entries {
            self.entries.remove(0);
        }
        self.save_if_configured();
    }

    /// List the last N entries (most recent last).
    #[must_use]
    pub fn list(&self, n: usize) -> &[HistoryEntry] {
        let len = self.entries.len();
        let start = len.saturating_sub(n);
        &self.entries[start..]
    }

    /// Search entries for entries containing the query (case-insensitive).
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<&HistoryEntry> {
        let q = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.cmd.to_lowercase().contains(&q)
                    || e.args.iter().any(|a| a.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Load history from the configured file.
    pub fn load(&mut self) -> std::io::Result<()> {
        let Some(ref path) = self.file_path else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let content = std::fs::read_to_string(path)?;
        let entries: Vec<HistoryEntry> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        self.entries = entries;
        // Trim to max
        if self.entries.len() > self.max_entries {
            let excess = self.entries.len() - self.max_entries;
            self.entries.drain(..excess);
        }
        Ok(())
    }

    /// Save history to the configured file.
    fn save_if_configured(&self) {
        let Some(ref path) = self.file_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let content: String = self
            .entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(path, content);
    }

    /// Get the total number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if history is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hist = CommandHistory::new(5);
        hist.set_file_path(tmp.path().join("history.jsonl"));

        hist.append("clear", &[]);
        hist.append("model", &["claude-opus".into()]);
        hist.append("status", &[]);

        let last2 = hist.list(2);
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0].cmd, "model");
        assert_eq!(last2[1].cmd, "status");
    }

    #[test]
    fn ring_buffer_trims_old() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hist = CommandHistory::new(3);
        hist.set_file_path(tmp.path().join("history.jsonl"));

        hist.append("cmd1", &[]);
        hist.append("cmd2", &[]);
        hist.append("cmd3", &[]);
        hist.append("cmd4", &[]);

        assert_eq!(hist.len(), 3);
        let all = hist.list(10);
        assert_eq!(all[0].cmd, "cmd2");
        assert_eq!(all[2].cmd, "cmd4");
    }

    #[test]
    fn search_finds_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hist = CommandHistory::new(100);
        hist.set_file_path(tmp.path().join("history.jsonl"));

        hist.append("model", &["sonnet".into()]);
        hist.append("status", &[]);
        hist.append("model", &["opus".into()]);

        let results = hist.search("model");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn load_and_save_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");

        let mut hist1 = CommandHistory::new(100);
        hist1.set_file_path(path.clone());
        hist1.append("test", &["arg1".into()]);
        hist1.append("clear", &[]);

        let mut hist2 = CommandHistory::new(100);
        hist2.set_file_path(path);
        hist2.load().unwrap();
        assert_eq!(hist2.len(), 2);
        assert_eq!(hist2.list(2)[0].cmd, "test");
        assert_eq!(hist2.list(2)[0].args, vec!["arg1"]);
    }

    #[test]
    fn load_with_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hist = CommandHistory::new(100);
        hist.set_file_path(tmp.path().join("nonexistent.jsonl"));
        hist.load().unwrap();
        assert!(hist.is_empty());
    }
}
