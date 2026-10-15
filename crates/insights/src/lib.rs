//! CodeRex insights — what the sessions on this machine add up to.
//!
//! The report answers three questions from transcripts alone: what the work
//! actually was, where it went wrong, and what is worth changing. It is built
//! from files already on disk and writes a file back; nothing is uploaded, and
//! nothing here calls a model — the counts are counted, and the suggestions
//! say which counts they came from.
//!
//! CodeRex keeps transcripts per workspace (`<cwd>/.coderex/transcripts`), so
//! insights are per workspace too: a report covers the sessions of the project
//! it was generated in.

#![warn(missing_docs)]

pub mod aggregate;
pub mod facts;
pub mod report;
pub mod store;

use std::path::{Path, PathBuf};

pub use aggregate::{Aggregate, Finding, FindingKind, aggregate, findings};
pub use facts::SessionFacts;
pub use store::{InsightsStore, StoredReport, default_root};

/// A generated report and what it says.
#[derive(Debug, Clone)]
pub struct InsightsOutcome {
    /// Where the report was written.
    pub report: StoredReport,
    /// The numbers behind it.
    pub aggregate: Aggregate,
    /// What they suggest.
    pub findings: Vec<Finding>,
    /// How many sessions were read.
    pub sessions_read: usize,
    /// Transcript directories that could not be listed.
    pub unreadable: Vec<String>,
}

impl InsightsOutcome {
    /// One line for a person, before they open the page.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.sessions_read == 0 {
            return "No sessions to report on yet.".to_string();
        }
        format!(
            "{} session(s), {} message(s), {} tool call(s) ({} failed).",
            self.aggregate.sessions,
            self.aggregate.messages,
            self.aggregate.total_tool_calls(),
            self.aggregate.total_tool_errors()
        )
    }
}

/// Read the given transcript directories and write a report.
///
/// # Errors
///
/// Fails if the report cannot be written. A directory that cannot be listed is
/// recorded in `unreadable` rather than failing the run: one unreadable
/// workspace should not cost the report on the rest.
pub fn run(
    transcript_dirs: &[PathBuf],
    store_root: impl Into<PathBuf>,
) -> Result<InsightsOutcome, store::InsightsError> {
    let mut sessions = Vec::new();
    let mut unreadable = Vec::new();

    for dir in transcript_dirs {
        match facts::read_dir(dir) {
            Ok(mut found) => sessions.append(&mut found),
            Err(error) => {
                unreadable.push(format!("{}: {error}", dir.display()));
            }
        }
    }

    let aggregate = aggregate::aggregate(&sessions);
    let findings = aggregate::findings(&aggregate);
    let html = report::render(&aggregate, &findings, &sessions);

    let store = InsightsStore::open(store_root);
    let report = store.write_report(&html)?;
    store.write_facets(&sessions)?;

    Ok(InsightsOutcome {
        report,
        aggregate,
        findings,
        sessions_read: sessions.len(),
        unreadable,
    })
}

/// The transcript directory a workspace keeps.
#[must_use]
pub fn workspace_transcripts(cwd: &Path) -> PathBuf {
    cwd.join(".coderex").join("transcripts")
}
