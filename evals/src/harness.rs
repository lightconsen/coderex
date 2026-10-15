//! The harness — what a flow declares, and how it is pinned.
//!
//! A flow is a directory holding `flow.json`: the command one case runs, the
//! cases themselves, and the defaults a run inherits. Everything the run does
//! is data, because the point of the scaffold is that adding `v2` is a new
//! variant of the *same* harness — not a new program.
//!
//! The harness is also what a run is approved against: its hash goes into
//! `_state.json`, and a run whose harness no longer matches the approved hash
//! is refused. Otherwise a "variant" could quietly mean a different test.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths::{HarnessError, assert_in_flow, path_safe_id};

/// The harness file inside a flow directory.
pub const HARNESS_FILE: &str = "flow.json";

/// Where the hill-climb loop keeps its state inside a repository.
pub const HILLCLIMB_DIR: &str = ".coderex/hillclimb";

/// File the run records are appended to.
pub const RESULTS_FILE: &str = "results.jsonl";

/// File holding the approved harness hash.
pub const STATE_FILE: &str = "_state.json";

/// The generated report.
pub const REPORT_FILE: &str = "report.html";

/// What a flow declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Harness {
    /// What the flow is called.
    pub name: String,
    /// One line about what it measures.
    #[serde(default)]
    pub description: String,
    /// The command one case runs.
    ///
    /// Placeholders: `{id}`, `{prompt}`, `{variant}`, `{flow}`, `{cwd}`,
    /// `{model}`.
    pub command: String,
    /// Cases, in the order they are listed.
    #[serde(default)]
    pub cases: Vec<Case>,
    /// Default repetitions per case.
    #[serde(default = "default_reps")]
    pub reps: u32,
    /// Default concurrency.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Default per-run timeout in seconds; 0 means no ceiling.
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
}

fn default_reps() -> u32 {
    1
}

fn default_concurrency() -> usize {
    4
}

fn default_timeout() -> u64 {
    300
}

/// One case, and what it must do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    /// Stable id, also used in file names, so it has to be path-safe.
    pub id: String,
    /// What the case is about, for the report.
    #[serde(default)]
    pub description: String,
    /// The prompt handed to the command, if the command takes one.
    #[serde(default)]
    pub prompt: String,
    /// What has to be true afterwards.
    #[serde(default)]
    pub expect: Expectations,
}

/// What a case has to do to pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expectations {
    /// Required exit code, if any.
    pub exit_code: Option<i32>,
    /// Substrings that must appear in the output.
    #[serde(default)]
    pub output_contains: Vec<String>,
    /// Substrings that must not appear in the output.
    #[serde(default)]
    pub output_not_contains: Vec<String>,
    /// Files that must exist, and what they must contain.
    #[serde(default)]
    pub files_contain: BTreeMap<String, String>,
    /// Files that must not exist.
    #[serde(default)]
    pub files_absent: Vec<String>,
}

impl Harness {
    /// Load a flow's harness.
    pub fn load(flow_dir: &Path) -> Result<Self, HarnessError> {
        let path = flow_dir.join(HARNESS_FILE);
        let bytes = std::fs::read(&path).map_err(|error| HarnessError::Read {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        let harness: Self =
            serde_json::from_slice(&bytes).map_err(|error| HarnessError::Parse {
                path: path.display().to_string(),
                message: error.to_string(),
            })?;
        harness.validate()?;
        Ok(harness)
    }

    /// Check the harness is usable before anything is run.
    pub fn validate(&self) -> Result<(), HarnessError> {
        if self.name.trim().is_empty() {
            return Err(HarnessError::Invalid("the flow needs a name".to_string()));
        }
        if self.command.trim().is_empty() {
            return Err(HarnessError::Invalid(
                "the flow needs a command to run each case".to_string(),
            ));
        }
        if self.cases.is_empty() {
            return Err(HarnessError::Invalid(
                "the flow has no cases to run".to_string(),
            ));
        }
        if self.reps == 0 {
            return Err(HarnessError::Invalid("reps must be at least 1".to_string()));
        }
        if self.concurrency == 0 {
            return Err(HarnessError::Invalid(
                "concurrency must be at least 1".to_string(),
            ));
        }

        let mut seen = std::collections::HashSet::new();
        for case in &self.cases {
            path_safe_id(&case.id)?;
            if !seen.insert(case.id.as_str()) {
                return Err(HarnessError::Invalid(format!(
                    "two cases share the id '{}'",
                    case.id
                )));
            }
        }
        Ok(())
    }

    /// The command for one case, with the placeholders filled in.
    #[must_use]
    pub fn command_for(&self, case: &Case, variant: &str, flow_dir: &Path, cwd: &Path) -> String {
        self.command
            .replace("{id}", &case.id)
            .replace("{prompt}", &case.prompt)
            .replace("{variant}", variant)
            .replace("{flow}", &flow_dir.display().to_string())
            .replace("{cwd}", &cwd.display().to_string())
    }

    /// Look a case up by id.
    #[must_use]
    pub fn case(&self, id: &str) -> Option<&Case> {
        self.cases.iter().find(|case| case.id == id)
    }

    /// Hash the harness files in a flow directory.
    ///
    /// Every file the harness reads is hashed, not just `flow.json`: a case
    /// that moved into its own file is still part of what a run was approved
    /// against, and a hash that ignored it would let it change unseen.
    pub fn hash(flow_dir: &Path) -> Result<String, HarnessError> {
        let files = harness_files(flow_dir)?;
        let mut hasher = Sha256::new();
        for (name, contents) in files {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            hasher.update((contents.len() as u64).to_le_bytes());
            hasher.update(&contents);
        }
        Ok(hex(&hasher.finalize()))
    }

    /// The files a harness hash covers, sorted by name.
    pub fn files(flow_dir: &Path) -> Result<Vec<String>, HarnessError> {
        Ok(harness_files(flow_dir)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }
}

/// Every file in a flow that is part of the harness, sorted by name.
///
/// A variant's output directories are skipped: they are results, not harness,
/// and hashing them would make the harness change every time it is run.
fn harness_files(flow_dir: &Path) -> Result<Vec<(String, Vec<u8>)>, HarnessError> {
    let mut files = Vec::new();
    collect(flow_dir, flow_dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn collect(
    root: &Path,
    dir: &Path,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), HarnessError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(HarnessError::Read {
                path: dir.display().to_string(),
                message: error.to_string(),
            });
        }
    };

    for entry in entries {
        let entry = entry.map_err(|error| HarnessError::Read {
            path: dir.display().to_string(),
            message: error.to_string(),
        })?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if path.is_dir() {
            if is_output_dir(&name) {
                continue;
            }
            collect(root, &path, files)?;
        } else if name != STATE_FILE && name != RESULTS_FILE && name != REPORT_FILE {
            let contents = std::fs::read(&path).map_err(|error| HarnessError::Read {
                path: path.display().to_string(),
                message: error.to_string(),
            })?;
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            files.push((relative, contents));
        }
    }
    Ok(())
}

/// Whether a directory inside a flow is run output rather than harness.
fn is_output_dir(name: &str) -> bool {
    name == "baseline"
        || name == "ref"
        || (name.len() > 1
            && name.starts_with('v')
            && name[1..].chars().all(|c| c.is_ascii_digit()))
}

/// The variants a flow accepts: `baseline`, `ref/<id>`, and `v<N>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Variant {
    /// The starting point every variant is measured against.
    Baseline,
    /// A numbered variant.
    Numbered(u32),
    /// A reference result kept beside the variants.
    Reference(String),
}

impl Variant {
    /// Parse a variant id.
    pub fn parse(id: &str) -> Result<Self, HarnessError> {
        if id == "baseline" {
            return Ok(Self::Baseline);
        }
        if let Some(name) = id.strip_prefix("ref/") {
            return Ok(Self::Reference(assert_in_flow(name)?.to_string()));
        }
        if let Some(number) = id.strip_prefix('v') {
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                let number = number.parse().map_err(|_| {
                    HarnessError::Invalid(format!("'{id}' is not a usable variant number"))
                })?;
                return Ok(Self::Numbered(number));
            }
        }
        Err(HarnessError::Invalid(format!(
            "variant '{id}' must be 'baseline' or 'v<N>'"
        )))
    }

    /// The id as it is written in file names.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::Baseline => "baseline".to_string(),
            Self::Numbered(number) => format!("v{number}"),
            Self::Reference(name) => format!("ref/{name}"),
        }
    }
}

/// The flow directory for a repository.
#[must_use]
pub fn flow_dir(repo: &Path, flow: &str) -> PathBuf {
    repo.join(HILLCLIMB_DIR).join(flow)
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

    fn flow(dir: &Path) -> PathBuf {
        let flow = dir.join("flow");
        std::fs::create_dir_all(&flow).unwrap();
        std::fs::write(
            flow.join(HARNESS_FILE),
            r#"{
                "name": "smoke",
                "command": "echo {prompt}",
                "cases": [
                    {"id": "c1", "prompt": "hello", "expect": {"exit_code": 0, "output_contains": ["hello"]}},
                    {"id": "c2", "prompt": "bye"}
                ]
            }"#,
        )
        .unwrap();
        flow
    }

    #[test]
    fn a_harness_loads_and_fills_in_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let flow = flow(dir.path());
        let harness = Harness::load(&flow).unwrap();

        assert_eq!(harness.name, "smoke");
        assert_eq!(harness.cases.len(), 2);
        assert_eq!(harness.reps, 1);
        assert_eq!(harness.concurrency, 4);
        assert_eq!(harness.timeout_s, 300);

        let case = harness.case("c1").unwrap();
        assert_eq!(
            harness.command_for(case, "v1", &flow, dir.path()),
            "echo hello"
        );
    }

    #[test]
    fn a_harness_without_cases_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let flow = dir.path().join("flow");
        std::fs::create_dir_all(&flow).unwrap();
        std::fs::write(
            flow.join(HARNESS_FILE),
            r#"{"name": "empty", "command": "true", "cases": []}"#,
        )
        .unwrap();

        let error = Harness::load(&flow).unwrap_err();
        assert!(error.to_string().contains("no cases"), "{error}");
    }

    #[test]
    fn a_case_id_that_is_not_path_safe_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let flow = dir.path().join("flow");
        std::fs::create_dir_all(&flow).unwrap();
        std::fs::write(
            flow.join(HARNESS_FILE),
            r#"{"name": "bad", "command": "true", "cases": [{"id": "../escape"}]}"#,
        )
        .unwrap();

        assert!(Harness::load(&flow).is_err());
    }

    #[test]
    fn two_cases_cannot_share_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let flow = dir.path().join("flow");
        std::fs::create_dir_all(&flow).unwrap();
        std::fs::write(
            flow.join(HARNESS_FILE),
            r#"{"name": "dup", "command": "true", "cases": [{"id": "c"}, {"id": "c"}]}"#,
        )
        .unwrap();

        let error = Harness::load(&flow).unwrap_err();
        assert!(error.to_string().contains("share the id"), "{error}");
    }

    #[test]
    fn the_hash_covers_the_harness_and_ignores_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let flow = flow(dir.path());
        let before = Harness::hash(&flow).unwrap();

        std::fs::create_dir_all(flow.join("v1")).unwrap();
        std::fs::write(flow.join("v1/results.jsonl"), b"{}").unwrap();
        std::fs::write(flow.join(STATE_FILE), b"{}").unwrap();
        std::fs::write(flow.join(REPORT_FILE), b"<html></html>").unwrap();
        assert_eq!(Harness::hash(&flow).unwrap(), before);

        std::fs::write(flow.join("cases.json"), b"[]").unwrap();
        assert_ne!(Harness::hash(&flow).unwrap(), before);
    }

    #[test]
    fn the_hash_notices_an_edit_to_the_harness() {
        let dir = tempfile::tempdir().unwrap();
        let flow = flow(dir.path());
        let before = Harness::hash(&flow).unwrap();

        let path = flow.join(HARNESS_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("\"hello\"", "\"hello there\"")).unwrap();

        assert_ne!(Harness::hash(&flow).unwrap(), before);
    }

    #[test]
    fn variants_parse_only_in_the_agreed_shapes() {
        assert_eq!(Variant::parse("baseline").unwrap(), Variant::Baseline);
        assert_eq!(Variant::parse("v2").unwrap(), Variant::Numbered(2));
        assert_eq!(
            Variant::parse("ref/agent-a").unwrap(),
            Variant::Reference("agent-a".into())
        );

        for bad in [
            "",
            "v",
            "vx",
            "V1",
            "variant1",
            "../baseline",
            "ref/..",
            "ref/",
        ] {
            assert!(Variant::parse(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn the_flow_directory_is_under_the_repository() {
        assert_eq!(
            flow_dir(Path::new("/repo"), "smoke"),
            PathBuf::from("/repo/.coderex/hillclimb/smoke")
        );
    }
}
