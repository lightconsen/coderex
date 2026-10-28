//! The `Workflow` tool — the surface a model reaches the runtime through.
//!
//! A workflow is what a *model* writes, and it spawns other agents, so the
//! tool is where the two things that make that safe live: the opt-in, and the
//! fact that the script is written down before it runs. A run leaves its
//! script and its journal in a directory of its own, so a resumed run can be
//! resumed from a different session, and so the thing that ran is the thing
//! that can be read afterwards.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use coderex_tools::{
    InterruptBehavior, Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolOutput,
    ToolTier,
};

use crate::meta::MAX_SCRIPT_BYTES;
use crate::runtime::{AgentRunner, Limits, run_script};

/// The tool's name.
pub const WORKFLOW_TOOL: &str = "Workflow";

/// The environment variable that turns workflow scripts on.
pub const ENABLE_FLAG: &str = "workflow_scripts";

/// The environment variable that overrides the concurrency gate.
pub const MAX_CONCURRENT_ENV: &str = "CODEREX_WORKFLOW_MAX_CONCURRENT_AGENTS";

/// Environment variable holding the run's output-token budget.
pub const TOKEN_BUDGET_ENV: &str = "CODEREX_WORKFLOW_TOKEN_BUDGET";

/// Longest a run's result is reported at, in characters.
pub const MAX_RESULT_CHARS: usize = 16_000;

/// Longest a `resumeFromRunId` may be.
const MAX_RUN_ID_CHARS: usize = 64;

/// Where workflows live when nothing else is configured.
#[must_use]
pub fn default_root() -> PathBuf {
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coderex")
        .join("workflows")
}

/// The `Workflow` tool.
#[derive(Clone)]
pub struct WorkflowTool {
    runner: Arc<dyn AgentRunner>,
    root: PathBuf,
    enabled: bool,
    /// Where a `name` is looked up, beyond the user's workflow directory.
    project_dir: Option<PathBuf>,
    /// Scripts contributed by plugins and other embedders, keyed by name.
    ///
    /// These are looked up before `project_dir` and the user's workflow
    /// directory, so a host can supply a workflow without writing a file.
    ///
    /// Shared so a session can contribute scripts after the tool has been
    /// registered (plugins load after the registry is built).
    named_scripts: Arc<std::sync::RwLock<HashMap<String, String>>>,
}

impl std::fmt::Debug for WorkflowTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowTool")
            .field("root", &self.root)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl WorkflowTool {
    /// Build the tool over a runner.
    #[must_use]
    pub fn new(runner: Arc<dyn AgentRunner>) -> Self {
        Self {
            runner,
            root: default_root(),
            // Off until a session says otherwise: a workflow spawns agents,
            // and that is not something to discover by accident.
            enabled: false,
            project_dir: None,
            named_scripts: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Store scripts and journals somewhere other than the default location.
    #[must_use]
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = root;
        self
    }

    /// Turn the tool on for this session.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Also look for named workflows in this directory.
    #[must_use]
    pub fn with_project_dir(mut self, dir: PathBuf) -> Self {
        self.project_dir = Some(dir);
        self
    }

    /// Register scripts that can be run by `name`, without a file on disk.
    ///
    /// A host (a plugin, an embedder) supplies a map from name to script
    /// source. Registered names are looked up before the project and user
    /// workflow directories; explicit `scriptPath` and `script` arguments
    /// still take priority over a name.
    ///
    /// Invalid names — empty, longer than 64 characters, or containing
    /// anything other than ASCII letters, digits, `-`, and `_` (so no path
    /// separators) — are skipped with a warning rather than registered. The
    /// rest of the map is still registered.
    #[must_use]
    pub fn with_named_scripts(self, scripts: HashMap<String, String>) -> Self {
        self.named_scripts()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(scripts.into_iter().filter(|(name, _)| {
                if is_workflow_name(name) {
                    true
                } else {
                    tracing::warn!(
                        name = %name,
                        "ignoring a workflow script registered under an invalid name"
                    );
                    false
                }
            }));
        self
    }

    /// The shared named-script map, for a host that registers scripts later.
    #[must_use]
    pub fn named_scripts(&self) -> Arc<std::sync::RwLock<HashMap<String, String>>> {
        Arc::clone(&self.named_scripts)
    }

    /// Register one named script after construction.
    pub fn register_named_script(&self, name: &str, script: impl Into<String>) {
        if is_workflow_name(name) {
            self.named_scripts
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(name.to_string(), script.into());
        } else {
            tracing::warn!(
                name = %name,
                "ignoring a workflow script registered under an invalid name"
            );
        }
    }

    /// Where a run's script and journal are kept.
    #[must_use]
    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join("runs").join(run_id)
    }

    fn limits() -> Limits {
        let mut limits = Limits::default();
        if let Ok(value) = std::env::var(MAX_CONCURRENT_ENV)
            && let Ok(parsed) = value.parse::<usize>()
        {
            limits.max_concurrent = parsed.clamp(1, 64);
        }
        if let Ok(value) = std::env::var(TOKEN_BUDGET_ENV)
            && let Ok(parsed) = value.parse::<u64>()
        {
            limits.token_budget = Some(parsed);
        }
        limits
    }

    /// Find the script a call is asking to run.
    fn resolve(&self, arguments: &Value) -> Result<Resolved, ToolError> {
        if let Some(run_id) = string_argument(arguments, "resumeFromRunId") {
            if !is_run_id(&run_id) {
                return Err(invalid(&format!(
                    "'{run_id}' is not a workflow run id (they look like `wf_1a2b3c4d-5e6`)"
                )));
            }
            let run_dir = self.run_dir(&run_id);
            let script_path = run_dir.join("script.js");
            let script = std::fs::read_to_string(&script_path).map_err(|error| {
                invalid(&format!("run '{run_id}' has no script to resume: {error}"))
            })?;
            return Ok(Resolved {
                script,
                run_id,
                run_dir,
                resumed: true,
            });
        }

        // Precedence: an explicit `scriptPath`, then an inline `script`, then a
        // registered name, then a name on disk in the project or user
        // directory. The first thing supplied wins.
        let script = if let Some(path) = string_argument(arguments, "scriptPath") {
            let path = Path::new(&path);
            if path.extension().is_some_and(|extension| extension != "js") {
                return Err(invalid(
                    "a workflow script is plain JavaScript, not TypeScript",
                ));
            }
            std::fs::read_to_string(path)
                .map_err(|error| invalid(&format!("cannot read {}: {error}", path.display())))?
        } else if let Some(inline) = string_argument(arguments, "script") {
            inline
        } else if let Some(name) = string_argument(arguments, "name") {
            let registered = self
                .named_scripts
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&name)
                .cloned();
            if let Some(script) = registered {
                script
            } else {
                self.find_named(&name)?
            }
        } else {
            return Err(invalid(
                "a workflow call needs `script`, `scriptPath`, `name`, or `resumeFromRunId`",
            ));
        };

        if script.len() > MAX_SCRIPT_BYTES {
            return Err(invalid(&format!(
                "the script is {} bytes; the limit is {MAX_SCRIPT_BYTES}",
                script.len()
            )));
        }
        if script.chars().any(is_hidden) {
            return Err(invalid(
                "the script contains control or direction characters that would be hidden when \
                 it is shown",
            ));
        }

        let run_id = crate::runtime::new_run_id();
        let run_dir = self.run_dir(&run_id);
        Ok(Resolved {
            script,
            run_id,
            run_dir,
            resumed: false,
        })
    }

    /// Read a script by name, from the project first and then the user.
    fn find_named(&self, name: &str) -> Result<String, ToolError> {
        use std::fmt::Write as _;

        if !is_workflow_name(name) {
            return Err(invalid(&format!("'{name}' is not a workflow name")));
        }

        let mut candidates = Vec::new();
        if let Some(dir) = &self.project_dir {
            candidates.push(dir.join(format!("{name}.js")));
        }
        candidates.push(self.root.join("scripts").join(format!("{name}.js")));

        for candidate in &candidates {
            if let Ok(script) = std::fs::read_to_string(candidate) {
                return Ok(script);
            }
        }

        let mut message = format!(
            "no workflow named '{name}' — looked in {}",
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        {
            let scripts = self
                .named_scripts
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !scripts.is_empty() {
                // Sorted so the message does not depend on hash iteration order.
                let mut names: Vec<&str> = scripts.keys().map(String::as_str).collect();
                names.sort_unstable();
                let _ = write!(message, "; registered names: {}", names.join(", "));
            }
            drop(scripts);
        }
        Err(invalid(&message))
    }
}

/// Whether a string can be used as a workflow name.
///
/// Names are file stems, so they must be non-empty, no longer than 64
/// characters, and must not contain path separators or any other character
/// outside ASCII letters, digits, `-`, and `_`.
fn is_workflow_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What a call resolved to.
struct Resolved {
    script: String,
    run_id: String,
    run_dir: PathBuf,
    resumed: bool,
}

#[async_trait::async_trait]
impl Tool for WorkflowTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: WORKFLOW_TOOL.to_string(),
            tier: ToolTier::Baseline,
            input_schema: json!({
                "type": "object",
                "description": "orchestrate sub-agents with deterministic JavaScript workflow",
                "properties": {
                    "script": { "type": "string", "description": "the workflow, starting with `export const meta = { name, description, phases }`" },
                    "scriptPath": { "type": "string", "description": "a .js file to run; takes priority over script and name" },
                    "name": { "type": "string", "description": "a saved workflow's name" },
                    "args": { "description": "passed to the script as `args`, verbatim" },
                    "resumeFromRunId": { "type": "string", "description": "resume a run; agents whose calls did not change are replayed" }
                }
            }),
            output_schema: json!({ "type": "object", "properties": { "output": { "type": "string" } } }),
            requires_approval: false,
            is_enabled: self.enabled,
            is_read_only: false,
            // A run spawns agents and writes a journal; two at once would be
            // two sets of agents against one run directory.
            is_concurrency_safe: false,
            interrupt_behavior: InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        use std::fmt::Write as _;

        if !self.enabled {
            return Err(ToolError::InvalidInput {
                message: format!(
                    "workflow scripts are not enabled in this session — set \
                     CODEREX_FEATURE_FLAGS=+{ENABLE_FLAG} to turn them on"
                ),
            });
        }

        let resolved = self.resolve(&input.arguments)?;

        // The script is written down before it runs, so a run can be resumed
        // from another session and read afterwards.
        std::fs::create_dir_all(&resolved.run_dir)
            .map_err(|error| execution(&format!("cannot create the run directory: {error}")))?;
        std::fs::write(resolved.run_dir.join("script.js"), &resolved.script)
            .map_err(|error| execution(&format!("cannot save the script: {error}")))?;

        let args = input.arguments.get("args").cloned();
        let mut outcome = run_script(
            &resolved.script,
            args,
            Arc::clone(&self.runner),
            resolved.run_dir.clone(),
            Self::limits(),
        )
        .await
        .map_err(|error| execution(&error.to_string()))?;

        // The run *is* the directory it lives in — that is what a resume names
        // and where the journal is. The runtime's own id is for callers that
        // drive it directly, without a directory.
        outcome.run_id.clone_from(&resolved.run_id);

        let result = outcome.result.as_ref().map_or_else(
            || "(the script returned nothing)".to_string(),
            |value| truncate(&value.to_string(), MAX_RESULT_CHARS),
        );

        let mut report = format!(
            "Workflow {} {}\nrun {}\nagents: {} ran, {} replayed, {} produced nothing\n",
            if resolved.resumed {
                "resumed"
            } else {
                "finished"
            },
            "",
            outcome.run_id,
            outcome.progress.agents_run,
            outcome.progress.agents_replayed,
            outcome.progress.agents_empty,
        );
        if !outcome.progress.phases.is_empty() {
            let _ = writeln!(report, "phases: {}", outcome.progress.phases.join(" → "));
        }
        if !outcome.progress.log.is_empty() {
            let _ = write!(report, "log:\n{}\n", outcome.progress.log.join("\n"));
        }
        let _ = writeln!(
            report,
            "script: {}",
            resolved.run_dir.join("script.js").display()
        );
        let _ = write!(report, "result:\n{result}");

        Ok(ToolOutput {
            output: report,
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Whether a character is one that would not be seen in the approval dialog.
///
/// Control characters, and the direction and zero-width characters that can
/// reorder or hide what a person is reading: a script is shown before it runs,
/// so it must be the script that is shown.
const fn is_hidden(ch: char) -> bool {
    if ch.is_control() && ch != '\n' && ch != '\t' && ch != '\r' {
        return true;
    }
    matches!(ch,
        '\u{200b}'..='\u{200f}'   // zero-width and marks
        | '\u{202a}'..='\u{202e}' // bidi overrides and isolates
        | '\u{2066}'..='\u{2069}' // bidi isolates
        | '\u{feff}'               // byte-order mark
    )
}

fn string_argument(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

/// Whether a string is shaped like a run id.
#[must_use]
pub fn is_run_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("wf_") else {
        return false;
    };
    if value.len() > MAX_RUN_ID_CHARS || rest.len() < 6 {
        return false;
    }
    rest.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n… (truncated at {limit} characters)")
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidInput {
        message: message.to_string(),
    }
}

fn execution(message: &str) -> ToolError {
    ToolError::Execution {
        name: WORKFLOW_TOOL.to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{AgentOutput, AgentRequest};
    use std::sync::Mutex;

    #[derive(Default)]
    struct CountingRunner {
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl AgentRunner for CountingRunner {
        async fn run(&self, request: AgentRequest) -> Result<AgentOutput, String> {
            *self.calls.lock().unwrap() += 1;
            Ok(AgentOutput::text(format!("handled {}", request.prompt)))
        }
    }

    fn tool(dir: &Path, enabled: bool) -> (WorkflowTool, Arc<CountingRunner>) {
        let runner = Arc::new(CountingRunner::default());
        (
            WorkflowTool::new(runner.clone())
                .with_root(dir.to_path_buf())
                .with_enabled(enabled),
            runner,
        )
    }

    async fn call(tool: &WorkflowTool, arguments: Value) -> Result<String, ToolError> {
        tool.execute(
            ToolInput {
                call_id: "call_1".into(),
                name: WORKFLOW_TOOL.into(),
                arguments,
            },
            ToolContext::new(coderex_tools::ExecutionContext::Normal, Path::new("/tmp")),
        )
        .await
        .map(|output| output.output)
    }

    const SCRIPT: &str = "export const meta = { name: 'test', description: 'a test' }\n\
                          phase('One')\nconst a = await agent('look')\nreturn a";

    #[tokio::test]
    async fn the_tool_is_off_until_the_session_turns_it_on() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, runner) = tool(dir.path(), false);

        let error = call(&tool, json!({"script": SCRIPT})).await.unwrap_err();
        assert!(error.to_string().contains("not enabled"), "{error}");
        assert!(error.to_string().contains(ENABLE_FLAG), "{error}");
        assert_eq!(*runner.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_call_without_a_script_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let error = call(&tool, json!({})).await.unwrap_err();
        assert!(error.to_string().contains("resumeFromRunId"), "{error}");
    }

    #[tokio::test]
    async fn an_inline_script_runs_and_is_written_down() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, runner) = tool(dir.path(), true);

        let output = call(&tool, json!({"script": SCRIPT})).await.unwrap();

        assert!(output.contains("Workflow finished"), "{output}");
        assert!(output.contains("handled look"), "{output}");
        assert!(output.contains("phases: One"), "{output}");
        assert!(output.contains("1 ran"), "{output}");
        assert_eq!(*runner.calls.lock().unwrap(), 1);

        // The script is on disk, under the run's own directory.
        let run_id = output
            .lines()
            .find_map(|line| line.strip_prefix("run "))
            .expect("a run id");
        let script = tool.run_dir(run_id).join("script.js");
        assert!(script.is_file(), "{script:?}");
        assert_eq!(std::fs::read_to_string(script).unwrap(), SCRIPT);
    }

    #[tokio::test]
    async fn a_run_resumes_from_its_id_and_replays_its_agents() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, runner) = tool(dir.path(), true);

        let first = call(&tool, json!({"script": SCRIPT})).await.unwrap();
        let run_id = first
            .lines()
            .find_map(|line| line.strip_prefix("run "))
            .unwrap()
            .to_string();
        assert_eq!(*runner.calls.lock().unwrap(), 1);

        let resumed = call(&tool, json!({"resumeFromRunId": run_id}))
            .await
            .unwrap();
        assert!(resumed.contains("Workflow resumed"), "{resumed}");
        assert!(resumed.contains("0 ran, 1 replayed"), "{resumed}");
        // Nothing ran the second time: the journal had the answer.
        assert_eq!(*runner.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn resuming_something_that_is_not_a_run_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let too_long = format!("wf_{}", "x".repeat(80));
        for bad in ["../etc/passwd", "wf_ab", "run-1", too_long.as_str()] {
            let error = call(&tool, json!({"resumeFromRunId": bad}))
                .await
                .unwrap_err();
            assert!(!error.to_string().is_empty(), "{bad} was accepted");
        }
    }

    #[tokio::test]
    async fn resuming_a_run_that_does_not_exist_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let error = call(&tool, json!({"resumeFromRunId": "wf_1a2b3c4d-5e6"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no script to resume"), "{error}");
    }

    #[tokio::test]
    async fn a_named_workflow_is_found_in_the_project_first() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("review.js"),
            "export const meta = { name: 'review', description: 'from the project' }\n\
             return await agent('project version')",
        )
        .unwrap();

        let (tool, runner) = tool(dir.path(), true);
        let tool = tool.with_project_dir(project);

        let output = call(&tool, json!({"name": "review"})).await.unwrap();
        assert!(output.contains("handled project version"), "{output}");
        assert_eq!(*runner.calls.lock().unwrap(), 1);
    }

    fn named(scripts: &[(&str, &str)]) -> HashMap<String, String> {
        scripts
            .iter()
            .map(|(name, script)| ((*name).to_string(), (*script).to_string()))
            .collect()
    }

    #[tokio::test]
    async fn a_registered_script_runs_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, runner) = tool(dir.path(), true);
        let script = "export const meta = { name: 'from-plugin', description: 'contributed' }\n\
                      return await agent('plugin version')";
        let tool = tool.with_named_scripts(named(&[("from-plugin", script)]));

        let output = call(&tool, json!({"name": "from-plugin"})).await.unwrap();

        assert!(output.contains("handled plugin version"), "{output}");
        assert!(output.contains("1 ran"), "{output}");
        assert_eq!(*runner.calls.lock().unwrap(), 1);

        // A registered script is written down like any other.
        let run_id = output
            .lines()
            .find_map(|line| line.strip_prefix("run "))
            .expect("a run id");
        assert_eq!(
            std::fs::read_to_string(tool.run_dir(run_id).join("script.js")).unwrap(),
            script
        );
    }

    #[tokio::test]
    async fn an_explicit_script_wins_over_a_registered_name() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, runner) = tool(dir.path(), true);
        let tool = tool.with_named_scripts(named(&[(
            "chosen",
            "export const meta = { name: 'a', description: 'b' }\nreturn await agent('plugin version')",
        )]));

        let output = call(
            &tool,
            json!({
                "name": "chosen",
                "script": "export const meta = { name: 'a', description: 'b' }\nreturn await agent('inline version')"
            }),
        )
        .await
        .unwrap();

        assert!(output.contains("handled inline version"), "{output}");
        assert!(!output.contains("plugin version"), "{output}");
        assert_eq!(*runner.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn an_explicit_script_path_wins_over_a_script_and_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let path_script = "export const meta = { name: 'a', description: 'b' }\n\
                           return await agent('path version')";
        let script_path = dir.path().join("path.js");
        std::fs::write(&script_path, path_script).unwrap();

        let (tool, runner) = tool(dir.path(), true);
        let tool = tool.with_named_scripts(named(&[(
            "chosen",
            "export const meta = { name: 'a', description: 'b' }\nreturn await agent('plugin version')",
        )]));

        let output = call(
            &tool,
            json!({
                "name": "chosen",
                "script": "export const meta = { name: 'a', description: 'b' }\nreturn await agent('inline version')",
                "scriptPath": script_path.to_string_lossy(),
            }),
        )
        .await
        .unwrap();

        assert!(output.contains("handled path version"), "{output}");
        assert_eq!(*runner.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_registered_script_wins_over_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("review.js"),
            "export const meta = { name: 'review', description: 'from the project' }\n\
             return await agent('project version')",
        )
        .unwrap();

        let (tool, runner) = tool(dir.path(), true);
        let tool = tool.with_project_dir(project).with_named_scripts(named(&[(
            "review",
            "export const meta = { name: 'review', description: 'from a plugin' }\n\
                 return await agent('plugin version')",
        )]));

        let output = call(&tool, json!({"name": "review"})).await.unwrap();
        assert!(output.contains("handled plugin version"), "{output}");
        assert_eq!(*runner.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn an_unknown_name_lists_the_registered_names() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);
        let tool = tool.with_named_scripts(named(&[
            ("zeta", "export const meta = {}"),
            ("alpha", "export const meta = {}"),
        ]));

        let error = call(&tool, json!({"name": "missing"})).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no workflow named 'missing'"), "{message}");
        assert!(
            message.contains("registered names: alpha, zeta"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn invalid_registered_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);
        let long = "x".repeat(65);
        let tool = tool.with_named_scripts(named(&[
            ("", "export const meta = {}"),
            ("../escape", "export const meta = {}"),
            ("with/slash", "export const meta = {}"),
            ("with\\backslash", "export const meta = {}"),
            (long.as_str(), "export const meta = {}"),
            ("good-name", "export const meta = {}"),
        ]));

        {
            let scripts = tool.named_scripts.read().unwrap();
            assert_eq!(scripts.len(), 1);
            assert!(scripts.contains_key("good-name"));
            drop(scripts);
        }

        // The refused names are not resolvable, and paths are still rejected.
        let error = call(&tool, json!({"name": "with/slash"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not a workflow name"), "{error}");
    }

    #[tokio::test]
    async fn a_name_that_is_not_there_names_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let error = call(&tool, json!({"name": "missing"})).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no workflow named 'missing'"), "{message}");
        assert!(message.contains("scripts"), "{message}");
    }

    #[tokio::test]
    async fn a_name_cannot_be_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let error = call(&tool, json!({"name": "../../etc/passwd"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not a workflow name"), "{error}");
    }

    #[tokio::test]
    async fn a_typescript_script_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("flow.ts"), "export const meta = {}").unwrap();
        let (tool, _) = tool(dir.path(), true);

        let error = call(
            &tool,
            json!({"scriptPath": dir.path().join("flow.ts").to_string_lossy()}),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("plain JavaScript"), "{error}");
    }

    #[tokio::test]
    async fn a_script_with_control_characters_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let script = format!("{SCRIPT}\u{202e}hidden");
        let error = call(&tool, json!({"script": script})).await.unwrap_err();
        assert!(error.to_string().contains("hidden"), "{error}");
    }

    #[tokio::test]
    async fn arguments_reach_the_script() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let output = call(
            &tool,
            json!({
                "script": "export const meta = { name: 'a', description: 'b' }\nreturn args.thing",
                "args": {"thing": "passed through"}
            }),
        )
        .await
        .unwrap();

        assert!(output.contains("passed through"), "{output}");
    }

    #[tokio::test]
    async fn a_script_that_fails_is_reported_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), true);

        let error = call(
            &tool,
            json!({"script": "export const meta = { name: 'a', description: 'b' }\nthrow new Error('gave up')"}),
        )
        .await
        .unwrap_err();

        match error {
            ToolError::Execution { message, name } => {
                assert_eq!(name, WORKFLOW_TOOL);
                assert!(message.contains("gave up"), "{message}");
            }
            other => panic!("expected an execution error, got {other:?}"),
        }
    }

    #[test]
    fn run_ids_are_recognised_by_shape() {
        assert!(is_run_id("wf_1a2b3c4d-5e6"));
        assert!(!is_run_id("wf_ab"));
        assert!(!is_run_id("1a2b3c4d"));
        assert!(!is_run_id("wf_1a2b3c4d-5e6/../x"));
        assert!(!is_run_id(&format!("wf_{}", "x".repeat(80))));
    }

    #[test]
    fn a_long_result_is_truncated_on_a_character_boundary() {
        let text = "é".repeat(20_000);
        let cut = truncate(&text, MAX_RESULT_CHARS);
        assert!(cut.contains("truncated"), "{}", &cut[cut.len() - 40..]);
        assert!(cut.chars().count() > MAX_RESULT_CHARS);
    }

    #[test]
    fn the_descriptor_is_off_when_the_session_has_not_opted_in() {
        let dir = tempfile::tempdir().unwrap();
        let (off, _) = tool(dir.path(), false);
        assert!(!off.descriptor().is_enabled);
        assert!(!off.descriptor().is_concurrency_safe);

        let (on, _) = tool(dir.path(), true);
        assert!(on.descriptor().is_enabled);
        assert_eq!(on.descriptor().name, WORKFLOW_TOOL);
    }
}
