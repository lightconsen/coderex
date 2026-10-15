//! Where the report and its caches are written.
//!
//! Two files per report, deliberately: a timestamped one, so a run does not
//! destroy the last one before anyone has read it, and a stable `report.html`
//! whose path can be bookmarked and handed to a browser. Facets are cached per
//! session beside them, so a later pass can read what extraction concluded
//! without reading every transcript again.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::facts::SessionFacts;

/// The directory name the report lives under.
pub const USAGE_DATA_DIR: &str = "usage-data";

/// The stable report file name.
pub const REPORT_FILE: &str = "report.html";

/// Something wrong writing the report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InsightsError {
    /// The report could not be written.
    #[error("cannot write the report: {0}")]
    Io(String),
}

/// A written report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredReport {
    /// The stable path, `report.html`.
    pub stable: PathBuf,
    /// The timestamped copy.
    pub timestamped: PathBuf,
    /// How big the report is.
    pub bytes: usize,
}

/// The store reports are written to.
#[derive(Debug, Clone)]
pub struct InsightsStore {
    root: PathBuf,
}

impl InsightsStore {
    /// Open the store at `root`.
    #[must_use]
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory reports live in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write the report, timestamped and stable.
    ///
    /// # Errors
    ///
    /// Fails if the directory or either file cannot be written.
    pub fn write_report(&self, html: &str) -> Result<StoredReport, InsightsError> {
        std::fs::create_dir_all(&self.root).map_err(|e| InsightsError::Io(e.to_string()))?;

        let timestamped = self.root.join(format!(
            "report-{}.html",
            chrono::Utc::now().format("%Y%m%d%H%M%S")
        ));
        std::fs::write(&timestamped, html).map_err(|e| InsightsError::Io(e.to_string()))?;

        let stable = self.root.join(REPORT_FILE);
        std::fs::write(&stable, html).map_err(|e| InsightsError::Io(e.to_string()))?;

        Ok(StoredReport {
            stable,
            timestamped,
            bytes: html.len(),
        })
    }

    /// Cache what each session contained.
    ///
    /// # Errors
    ///
    /// Fails if the facets directory cannot be written.
    pub fn write_facets(&self, sessions: &[SessionFacts]) -> Result<usize, InsightsError> {
        let dir = self.root.join("facets");
        std::fs::create_dir_all(&dir).map_err(|e| InsightsError::Io(e.to_string()))?;

        let mut written = 0;
        for session in sessions {
            // The id comes from a file name, so it is already path-safe; the
            // check is here so a caller that builds one by hand cannot escape.
            let Ok(name) = crate::store::safe_name(&session.session_id) else {
                continue;
            };
            let bytes =
                serde_json::to_vec_pretty(session).map_err(|e| InsightsError::Io(e.to_string()))?;
            std::fs::write(dir.join(format!("{name}.json")), bytes)
                .map_err(|e| InsightsError::Io(e.to_string()))?;
            written += 1;
        }
        Ok(written)
    }

    /// Read a cached session, if one was written.
    ///
    /// # Errors
    ///
    /// Fails if the file exists but cannot be read or parsed.
    pub fn read_facets(&self, session_id: &str) -> Result<Option<SessionFacts>, InsightsError> {
        let name = safe_name(session_id).map_err(InsightsError::Io)?;
        let path = self.root.join("facets").join(format!("{name}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| InsightsError::Io(e.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(InsightsError::Io(error.to_string())),
        }
    }
}

/// Where reports go when nothing else is configured.
#[must_use]
pub fn default_root() -> PathBuf {
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coderex")
        .join(USAGE_DATA_DIR)
}

/// Check a session id is usable as a file name.
fn safe_name(id: &str) -> Result<String, String> {
    if id.is_empty() || id.len() > 128 {
        return Err(format!("'{id}' is not usable as a file name"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!("'{id}' has characters a file name cannot carry"));
    }
    if id.starts_with('.') || id == ".." {
        return Err(format!("'{id}' is not usable as a file name"));
    }
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn session(id: &str) -> SessionFacts {
        SessionFacts {
            session_id: id.to_string(),
            last_active: None,
            messages: 3,
            by_role: BTreeMap::from([("user".to_string(), 1)]),
            first_prompt: "hello".into(),
            tool_calls: BTreeMap::new(),
            tool_errors: BTreeMap::new(),
            compactions: 0,
            unreadable_lines: 0,
        }
    }

    #[test]
    fn a_report_is_written_twice() {
        let dir = tempfile::tempdir().unwrap();
        let store = InsightsStore::open(dir.path());
        let report = store.write_report("<html>hi</html>").unwrap();

        assert!(report.stable.is_file());
        assert!(report.timestamped.is_file());
        assert_eq!(report.stable.file_name().unwrap(), REPORT_FILE);
        assert_ne!(report.stable, report.timestamped);
        assert_eq!(
            std::fs::read_to_string(&report.stable).unwrap(),
            "<html>hi</html>"
        );
    }

    #[test]
    fn facets_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = InsightsStore::open(dir.path());

        assert_eq!(
            store
                .write_facets(&[session("sess_1"), session("sess_2")])
                .unwrap(),
            2
        );
        assert_eq!(
            store.read_facets("sess_1").unwrap().unwrap().session_id,
            "sess_1"
        );
        assert!(store.read_facets("never_seen").unwrap().is_none());
    }

    #[test]
    fn a_session_id_that_is_not_a_file_name_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = InsightsStore::open(dir.path());

        let written = store
            .write_facets(&[session("fine"), session("../escape")])
            .unwrap();
        assert_eq!(written, 1);
        assert!(store.read_facets("../escape").is_err());
    }

    #[test]
    fn the_default_root_is_under_the_config_directory() {
        let root = default_root();
        assert!(root.ends_with(".coderex/usage-data"), "{root:?}");
    }
}
