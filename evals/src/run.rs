//! Running a flow — the part that makes the loop a loop.
//!
//! A run is: every case, `reps` times, at a bounded concurrency, with what
//! happened appended to `results.jsonl` as it goes. Records already in that
//! file are not run again, so an interrupted run resumes and a variant can be
//! extended case by case without redoing what already passed.
//!
//! The harness is checked against the approved hash before anything runs: a
//! variant is only comparable to its baseline if the same harness measured
//! both.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::grade::{RunOutput, expects_something, grade};
use crate::harness::{
    Case, HARNESS_FILE, Harness, REPORT_FILE, RESULTS_FILE, STATE_FILE, Variant, flow_dir,
};
use crate::paths::HarnessError;

/// How much of a command's output a record keeps.
const OUTPUT_TAIL_BYTES: usize = 4_096;

/// What a run was asked to do.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The repository the flow lives in.
    pub repo: PathBuf,
    /// The flow's name.
    pub flow: String,
    /// An explicit flow directory.
    ///
    /// When set it replaces the usual
    /// `<repo>/.coderex/hillclimb/<flow>` resolution, so a flow can be
    /// checked in wherever it belongs (for example under `evals/`).
    pub flow_dir: Option<PathBuf>,
    /// Which variant this run is for.
    pub variant: Variant,
    /// Override for the harness's repetition count.
    pub reps: Option<u32>,
    /// Override for the harness's concurrency.
    pub concurrency: Option<usize>,
    /// Override for the harness's timeout.
    pub timeout_s: Option<u64>,
    /// The model a command may pass through, if it uses `{model}`.
    pub model: Option<String>,
    /// Record the current harness as approved before running.
    pub approve_harness: bool,
    /// Run cases that already have a record.
    pub force: bool,
    /// Only run this one case.
    pub only_case: Option<String>,
}

impl RunOptions {
    /// A run of `flow` in `repo` for `variant`, with the harness's defaults.
    #[must_use]
    pub fn new(repo: impl Into<PathBuf>, flow: impl Into<String>, variant: Variant) -> Self {
        Self {
            repo: repo.into(),
            flow: flow.into(),
            flow_dir: None,
            variant,
            reps: None,
            concurrency: None,
            timeout_s: None,
            model: None,
            approve_harness: false,
            force: false,
            only_case: None,
        }
    }
}

/// One case, one repetition, and how it went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The case this is for.
    pub case_id: String,
    /// The variant it ran under.
    pub variant: String,
    /// Which repetition, counting from zero.
    pub rep: u32,
    /// Whether it passed.
    pub passed: bool,
    /// Whether the case asserted anything at all.
    pub graded: bool,
    /// Its exit code, if it exited on its own.
    pub exit_code: Option<i32>,
    /// How long it took, in milliseconds.
    pub duration_ms: u64,
    /// Everything that did not hold.
    pub failures: Vec<String>,
    /// The end of what it printed.
    pub output_tail: String,
}

/// What a run did.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The flow.
    pub flow: String,
    /// The variant.
    pub variant: String,
    /// The harness hash this run was measured against.
    pub harness_sha256: String,
    /// Records this run produced, not counting the ones it resumed.
    pub records: Vec<Record>,
    /// How many were skipped because they were already recorded.
    pub resumed: usize,
    /// Where the records live.
    pub results_path: PathBuf,
    /// Where the report was written.
    pub report_path: PathBuf,
}

impl RunOutcome {
    /// How many records passed.
    #[must_use]
    pub fn passed(&self) -> usize {
        self.records.iter().filter(|record| record.passed).count()
    }

    /// The pass rate across this run's records.
    // A display ratio over a handful of records, far below the range where an
    // f64 loses integer precision.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn pass_rate(&self) -> f64 {
        if self.records.is_empty() {
            return 1.0;
        }
        self.passed() as f64 / self.records.len() as f64
    }
}

/// The approved-harness state kept beside a flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct State {
    harness_sha256: String,
    approved_at: String,
    files: Vec<String>,
}

/// Run a flow.
///
/// # Errors
///
/// Fails if the harness is unusable, was changed since it was approved, or the
/// run itself could not be written down.
pub async fn run(options: &RunOptions) -> Result<RunOutcome, HarnessError> {
    let flow_dir = match &options.flow_dir {
        Some(dir) => dir.clone(),
        None if !options.flow.trim().is_empty() => flow_dir(&options.repo, &options.flow),
        None => {
            return Err(HarnessError::Invalid(
                "a run needs --flow or --flow-dir".to_string(),
            ));
        }
    };
    // `{flow}` is substituted into a command that runs with the case's run
    // directory as its cwd, so a relative flow path would not resolve there.
    let flow_dir = if flow_dir.is_absolute() {
        flow_dir
    } else {
        std::env::current_dir()
            .map_err(|error| HarnessError::Run(error.to_string()))?
            .join(flow_dir)
    };
    let harness = Harness::load(&flow_dir)?;

    // A flow given by directory does not have to repeat its name on the
    // command line; the harness already declares one.
    let flow_name = if options.flow.trim().is_empty() {
        harness.name.clone()
    } else {
        options.flow.clone()
    };

    let hash = Harness::hash(&flow_dir)?;
    check_harness(&flow_dir, &hash, options.approve_harness)?;

    let reps = options.reps.unwrap_or(harness.reps);
    let concurrency = options.concurrency.unwrap_or(harness.concurrency).max(1);
    let timeout = options.timeout_s.unwrap_or(harness.timeout_s);

    let variant_id = options.variant.id();
    let variant_dir = flow_dir.join(&variant_id);
    std::fs::create_dir_all(&variant_dir).map_err(|error| HarnessError::Run(error.to_string()))?;
    let results_path = variant_dir.join(RESULTS_FILE);

    // Resume: anything already recorded for this variant is not run again.
    let existing = read_records(&results_path)?;
    let recorded: std::collections::HashSet<(String, u32)> = existing
        .iter()
        .map(|record| (record.case_id.clone(), record.rep))
        .collect();

    let cases: Vec<Case> = harness
        .cases
        .iter()
        .filter(|case| {
            options
                .only_case
                .as_ref()
                .is_none_or(|only| &case.id == only)
        })
        .cloned()
        .collect();

    if cases.is_empty() {
        return Err(HarnessError::Invalid(format!(
            "no case matches {:?}",
            options.only_case.clone().unwrap_or_default()
        )));
    }

    let mut pending = Vec::new();
    let mut resumed = 0usize;
    for case in &cases {
        for rep in 0..reps {
            if !options.force && recorded.contains(&(case.id.clone(), rep)) {
                resumed += 1;
                continue;
            }
            pending.push((case.clone(), rep, variant_dir.clone()));
        }
    }

    let records = execute(
        pending,
        concurrency,
        timeout,
        harness.clone(),
        variant_id.clone(),
        flow_dir.clone(),
        options.repo.clone(),
        options.model.clone(),
    )
    .await?;

    if !records.is_empty() {
        append_records(&results_path, &records)?;
    }

    let all = read_records(&results_path)?;
    let report = crate::report::render(&harness, &all, &hash, &flow_name);
    let report_path = flow_dir.join(REPORT_FILE);
    std::fs::write(&report_path, report).map_err(|error| HarnessError::Run(error.to_string()))?;

    Ok(RunOutcome {
        flow: flow_name,
        variant: variant_id,
        harness_sha256: hash,
        records,
        resumed,
        results_path,
        report_path,
    })
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    pending: Vec<(Case, u32, PathBuf)>,
    concurrency: usize,
    timeout_s: u64,
    harness: Harness,
    variant: String,
    flow_dir: PathBuf,
    repo: PathBuf,
    model: Option<String>,
) -> Result<Vec<Record>, HarnessError> {
    use tokio::sync::Semaphore;

    let semaphore = std::sync::Arc::new(Semaphore::new(concurrency));
    let mut tasks = Vec::with_capacity(pending.len());

    for (case, rep, variant_dir) in pending {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| HarnessError::Run(error.to_string()))?;

        let harness = harness.clone();
        let variant = variant.clone();
        let flow_dir = flow_dir.clone();
        let repo = repo.clone();
        let model = model.clone();
        let run_dir = variant_dir.join(&case.id).join(format!("run-{rep}"));

        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            run_case(
                &harness,
                &case,
                rep,
                &variant,
                &flow_dir,
                &repo,
                &run_dir,
                timeout_s,
                model.as_deref(),
            )
            .await
        }));
    }

    let mut records = Vec::with_capacity(tasks.len());
    for task in tasks {
        match task.await {
            Ok(record) => records.push(record),
            Err(error) => return Err(HarnessError::Run(format!("a case task failed: {error}"))),
        }
    }
    records.sort_by(|a, b| a.case_id.cmp(&b.case_id).then(a.rep.cmp(&b.rep)));
    Ok(records)
}

#[allow(clippy::too_many_arguments)]
async fn run_case(
    harness: &Harness,
    case: &Case,
    rep: u32,
    variant: &str,
    flow_dir: &Path,
    repo: &Path,
    run_dir: &Path,
    timeout_s: u64,
    model: Option<&str>,
) -> Record {
    // A repetition starts from nothing: a case that reads what a previous run
    // left behind would measure the previous run.
    let _ = std::fs::remove_dir_all(run_dir);
    if let Err(error) = std::fs::create_dir_all(run_dir) {
        return Record {
            case_id: case.id.clone(),
            variant: variant.to_string(),
            rep,
            passed: false,
            graded: expects_something(&case.expect),
            exit_code: None,
            duration_ms: 0,
            failures: vec![format!("could not create the run directory: {error}")],
            output_tail: String::new(),
        };
    }

    let command = harness.command_for(case, variant, flow_dir, run_dir);
    // `{model}` is a placeholder the command may carry, not a format argument.
    #[allow(clippy::literal_string_with_formatting_args)]
    let command = command.replace("{model}", model.unwrap_or("default"));

    let started = Instant::now();
    let (exit_code, output, timed_out) = spawn(&command, run_dir, repo, timeout_s).await;
    // A case would have to run for ~584 million years to overflow this.
    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = started.elapsed().as_millis() as u64;

    let run_output = RunOutput {
        exit_code,
        output: output.clone(),
        cwd: run_dir.to_path_buf(),
        timed_out,
    };
    let verdict = grade(&case.expect, &run_output);

    Record {
        case_id: case.id.clone(),
        variant: variant.to_string(),
        rep,
        passed: verdict.passed,
        graded: expects_something(&case.expect),
        exit_code,
        duration_ms,
        failures: verdict.failures,
        output_tail: tail(&output, OUTPUT_TAIL_BYTES),
    }
}

/// Run the command, returning its exit code, output, and whether it timed out.
async fn spawn(
    command: &str,
    cwd: &Path,
    repo: &Path,
    timeout_s: u64,
) -> (Option<i32>, String, bool) {
    let mut shell = if cfg!(windows) {
        let mut shell = tokio::process::Command::new("cmd");
        shell.arg("/C");
        shell
    } else {
        let mut shell = tokio::process::Command::new("sh");
        shell.arg("-c");
        shell
    };

    shell
        .arg(command)
        .current_dir(cwd)
        .env("CODEREX_EVAL_RUN_DIR", cwd)
        .env("CODEREX_EVAL_REPO", repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // The child keeps this process's environment: a case that runs `coderex`
    // needs the same keys and paths a person would have.
    let child = match shell.spawn() {
        Ok(child) => child,
        Err(error) => return (None, format!("could not start the command: {error}"), false),
    };

    let collect = async {
        child
            .wait_with_output()
            .await
            .map(|output| {
                let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&stderr);
                }
                (output.status.code(), text)
            })
            .map_err(|error| error.to_string())
    };

    if timeout_s == 0 {
        return match collect.await {
            Ok((code, text)) => (code, text, false),
            Err(error) => (None, error, false),
        };
    }

    match tokio::time::timeout(Duration::from_secs(timeout_s), collect).await {
        Ok(Ok((code, text))) => (code, text, false),
        Ok(Err(error)) => (None, error, false),
        Err(_) => (None, format!("timed out after {timeout_s}s"), true),
    }
}

/// Check a run against the approved harness hash.
fn check_harness(flow_dir: &Path, hash: &str, approve: bool) -> Result<(), HarnessError> {
    let state_path = flow_dir.join(STATE_FILE);

    if approve {
        let state = State {
            harness_sha256: hash.to_string(),
            approved_at: chrono::Utc::now().to_rfc3339(),
            files: Harness::files(flow_dir)?,
        };
        let bytes = serde_json::to_vec_pretty(&state)
            .map_err(|error| HarnessError::Run(error.to_string()))?;
        std::fs::write(&state_path, bytes).map_err(|error| HarnessError::Run(error.to_string()))?;
        return Ok(());
    }

    match std::fs::read(&state_path) {
        Ok(bytes) => {
            let state: State = serde_json::from_slice(&bytes)
                .map_err(|error| HarnessError::Run(error.to_string()))?;
            if state.harness_sha256 != hash {
                return Err(HarnessError::HarnessChanged {
                    files: state.files.join(", "),
                });
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                flow = %flow_dir.display(),
                "this harness has not been approved; run with --approve-harness to pin it"
            );
            Ok(())
        }
        Err(error) => Err(HarnessError::Run(error.to_string())),
    }
}

fn read_records(path: &Path) -> Result<Vec<Record>, HarnessError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(HarnessError::Run(error.to_string())),
    };

    let mut records = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // A half-written last line is what an interrupted run leaves behind;
        // the record it was going to be simply has not happened.
        match serde_json::from_str::<Record>(line) {
            Ok(record) => records.push(record),
            Err(error) => tracing::warn!("skipping an unreadable result line: {error}"),
        }
    }
    Ok(records)
}

fn append_records(path: &Path, records: &[Record]) -> Result<(), HarnessError> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| HarnessError::Run(error.to_string()))?;

    for record in records {
        let line =
            serde_json::to_string(record).map_err(|error| HarnessError::Run(error.to_string()))?;
        writeln!(file, "{line}").map_err(|error| HarnessError::Run(error.to_string()))?;
    }
    Ok(())
}

fn tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut start = text.len() - limit;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Every record a flow has, across all its variants.
pub fn all_records(flow_dir: &Path) -> Result<Vec<Record>, HarnessError> {
    let entries = match std::fs::read_dir(flow_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(HarnessError::Run(error.to_string())),
    };

    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| HarnessError::Run(error.to_string()))?;
        if !entry.path().is_dir() {
            continue;
        }
        records.extend(read_records(&entry.path().join(RESULTS_FILE))?);
    }
    Ok(records)
}

/// The harness file name, re-exported for callers that build a flow by hand.
pub const HARNESS: &str = HARNESS_FILE;

#[cfg(test)]
mod tests {
    use super::*;

    /// A flow whose command is plain shell, so the harness itself is testable
    /// without running CodeRex.
    fn write_flow(dir: &Path, command: &str, cases: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(HARNESS_FILE),
            format!(
                r#"{{"name": "smoke", "command": "{command}", "cases": [{cases}], "concurrency": 2}}"#
            ),
        )
        .unwrap();
    }

    fn repo() -> (PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        write_flow(
            &flow_dir(&repo, "smoke"),
            "printf 'ran {id}\\n'",
            r#"{"id": "c1", "prompt": "a", "expect": {"exit_code": 0, "output_contains": ["ran c1"]}},
               {"id": "c2", "prompt": "b", "expect": {"exit_code": 0, "output_contains": ["ran c2"]}}"#,
        );
        (repo, dir)
    }

    #[tokio::test]
    async fn a_run_executes_every_case_and_records_it() {
        let (repo, _dir) = repo();
        let outcome = run(&RunOptions::new(&repo, "smoke", Variant::Baseline))
            .await
            .unwrap();

        assert_eq!(outcome.records.len(), 2);
        assert_eq!(outcome.passed(), 2);
        assert!(outcome.pass_rate() > 0.99);
        assert!(outcome.results_path.is_file());

        let html = std::fs::read_to_string(&outcome.report_path).unwrap();
        assert!(html.contains("smoke"), "{html}");
    }

    #[tokio::test]
    async fn a_flow_can_live_outside_the_repository_layout() {
        let dir = tempfile::tempdir().unwrap();
        let flow = dir.path().join("checked-in-flow");
        write_flow(
            &flow,
            "printf 'checked {id}\\n'",
            r#"{"id": "c1", "expect": {"output_contains": ["checked c1"]}}"#,
        );

        let mut options = RunOptions::new(dir.path(), "", Variant::Baseline);
        options.flow_dir = Some(flow.clone());
        let outcome = run(&options).await.unwrap();

        assert_eq!(outcome.flow, "smoke");
        assert_eq!(outcome.passed(), 1);
        assert!(flow.join(REPORT_FILE).is_file());
        assert!(!dir.path().join(crate::HILLCLIMB_DIR).exists());
    }

    #[tokio::test]
    async fn a_second_run_resumes_instead_of_repeating() {
        let (repo, _dir) = repo();
        run(&RunOptions::new(&repo, "smoke", Variant::Baseline))
            .await
            .unwrap();

        let again = run(&RunOptions::new(&repo, "smoke", Variant::Baseline))
            .await
            .unwrap();
        assert_eq!(again.records.len(), 0);
        assert_eq!(again.resumed, 2);

        let mut options = RunOptions::new(&repo, "smoke", Variant::Baseline);
        options.force = true;
        assert_eq!(run(&options).await.unwrap().records.len(), 2);
    }

    #[tokio::test]
    async fn repeating_a_case_records_one_result_per_repetition() {
        let (repo, _dir) = repo();
        let mut options = RunOptions::new(&repo, "smoke", Variant::Numbered(1));
        options.reps = Some(3);
        let outcome = run(&options).await.unwrap();

        assert_eq!(outcome.records.len(), 6);
        let reps: Vec<u32> = outcome
            .records
            .iter()
            .filter(|record| record.case_id == "c1")
            .map(|record| record.rep)
            .collect();
        assert_eq!(reps, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn a_failing_case_is_recorded_with_its_reasons() {
        let (repo, _dir) = repo();
        write_flow(
            &flow_dir(&repo, "failing"),
            "printf 'nothing useful'",
            r#"{"id": "c1", "expect": {"output_contains": ["published"]}}"#,
        );

        let outcome = run(&RunOptions::new(&repo, "failing", Variant::Baseline))
            .await
            .unwrap();
        assert_eq!(outcome.passed(), 0);
        assert!(outcome.records[0].failures[0].contains("does not contain"));
        assert!(outcome.records[0].output_tail.contains("nothing useful"));
    }

    #[tokio::test]
    async fn a_timeout_is_recorded_rather_than_hanging() {
        let (repo, _dir) = repo();
        write_flow(
            &flow_dir(&repo, "slow"),
            "sleep 30",
            r#"{"id": "c1", "expect": {"exit_code": 0}}"#,
        );

        let mut options = RunOptions::new(&repo, "slow", Variant::Baseline);
        options.timeout_s = Some(1);
        let outcome = run(&options).await.unwrap();

        assert!(!outcome.records[0].passed);
        assert!(outcome.records[0].failures[0].contains("timeout"));
    }

    #[tokio::test]
    async fn a_case_can_assert_on_the_files_it_wrote() {
        let (repo, _dir) = repo();
        write_flow(
            &flow_dir(&repo, "files"),
            "printf 'hello' > out.txt",
            r#"{"id": "c1", "expect": {"files_contain": {"out.txt": "hello"}, "files_absent": ["nope.txt"]}}"#,
        );

        let outcome = run(&RunOptions::new(&repo, "files", Variant::Baseline))
            .await
            .unwrap();
        assert!(outcome.records[0].passed, "{:?}", outcome.records[0]);
    }

    #[tokio::test]
    async fn the_command_runs_in_a_fresh_directory_each_time() {
        let (repo, _dir) = repo();
        write_flow(
            &flow_dir(&repo, "fresh"),
            "printf 'x' >> count.txt && wc -c < count.txt",
            r#"{"id": "c1", "expect": {"output_contains": ["1"]}}"#,
        );

        let mut options = RunOptions::new(&repo, "fresh", Variant::Baseline);
        options.reps = Some(3);
        let outcome = run(&options).await.unwrap();

        assert_eq!(outcome.passed(), 3, "{:?}", outcome.records);
    }

    #[tokio::test]
    async fn an_unapproved_harness_runs_and_an_approved_one_is_enforced() {
        let (repo, _dir) = repo();
        let flow = flow_dir(&repo, "smoke");

        // Nothing approved yet, so the first run goes ahead.
        run(&RunOptions::new(&repo, "smoke", Variant::Baseline))
            .await
            .unwrap();

        let mut approve = RunOptions::new(&repo, "smoke", Variant::Baseline);
        approve.approve_harness = true;
        approve.force = true;
        run(&approve).await.unwrap();

        // An edit afterwards is refused until it is approved too.
        let path = flow.join(HARNESS_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("ran {id}", "ran  {id}")).unwrap();

        let error = run(&RunOptions::new(&repo, "smoke", Variant::Baseline))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("harness changed"), "{error}");
        assert!(error.to_string().contains(HARNESS_FILE), "{error}");

        let mut again = RunOptions::new(&repo, "smoke", Variant::Baseline);
        again.approve_harness = true;
        assert!(run(&again).await.is_ok());
    }

    #[tokio::test]
    async fn one_case_can_be_run_on_its_own() {
        let (repo, _dir) = repo();
        let mut options = RunOptions::new(&repo, "smoke", Variant::Baseline);
        options.only_case = Some("c2".into());
        let outcome = run(&options).await.unwrap();

        assert_eq!(outcome.records.len(), 1);
        assert_eq!(outcome.records[0].case_id, "c2");
    }

    #[tokio::test]
    async fn a_case_that_does_not_exist_is_refused() {
        let (repo, _dir) = repo();
        let mut options = RunOptions::new(&repo, "smoke", Variant::Baseline);
        options.only_case = Some("nope".into());
        assert!(run(&options).await.is_err());
    }

    #[tokio::test]
    async fn a_variant_keeps_its_own_records() {
        let (repo, _dir) = repo();
        run(&RunOptions::new(&repo, "smoke", Variant::Baseline))
            .await
            .unwrap();
        run(&RunOptions::new(&repo, "smoke", Variant::Numbered(1)))
            .await
            .unwrap();

        let flow = flow_dir(&repo, "smoke");
        assert!(flow.join("baseline").join(RESULTS_FILE).is_file());
        assert!(flow.join("v1").join(RESULTS_FILE).is_file());
        assert_eq!(all_records(&flow).unwrap().len(), 4);

        // Variants do not resume each other.
        let again = run(&RunOptions::new(&repo, "smoke", Variant::Numbered(1)))
            .await
            .unwrap();
        assert_eq!(again.resumed, 2);
    }

    #[test]
    fn a_long_output_is_kept_from_the_end() {
        let text = format!("{}END", "x".repeat(10_000));
        let kept = tail(&text, 100);
        assert!(kept.ends_with("END"), "{kept}");
        assert!(kept.len() < 200);
    }
}
