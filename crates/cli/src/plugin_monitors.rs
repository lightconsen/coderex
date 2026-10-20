//! Runtime supervision for plugin `monitors`.
//!
//! A monitor is a background watch script a plugin asks the host to keep alive
//! for the session — "background watch scripts the host arms as persistent
//! Monitor tasks". Monitors sit at the same trust tier as hooks: they run
//! unsandboxed, for as long as the session lasts, and this process's provider
//! credentials are removed from the environment they inherit.
//!
//! Both declared document shapes are accepted. An object maps monitor names to
//! their definitions:
//!
//! ```json
//! {
//!   "watch": { "command": "watchexec", "args": ["-w", "src"] },
//!   "lint": { "script": "scripts/lint-watch.sh", "env": { "RUST_LOG": "info" } }
//! }
//! ```
//!
//! An array carries the name on each entry instead:
//!
//! ```json
//! [{ "name": "watch", "command": "watchexec" }]
//! ```
//!
//! A relative `script` path is resolved against the supplied working
//! directory. Invalid entries are logged and skipped: one malformed monitor,
//! or one that cannot be spawned, must not keep the session or the other
//! monitors from running.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use coderex_core::process_tree::{JobLimits, ProcessTree};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// Every live monitor child's process tree, so a signal handler can kill it.
///
/// `Drop` is not enough: SIGTERM takes `process::exit` and release builds use
/// `panic = "abort"`, neither of which runs destructors. The registry is the
/// only cleanup that survives those paths.
///
/// The value is a [`ProcessTree`], not a pid: on Windows a pid only names the
/// direct child, and `taskkill /T` is best-effort at reaching grandchildren.
/// The tree's Job Object kills all of them without spawning a helper process
/// from a signal handler.
static MONITOR_TREES: Mutex<Vec<(u64, Arc<ProcessTree>)>> = Mutex::new(Vec::new());

/// Ids are assigned monotonically so two monitors cannot unregister each
/// other when a pid is reused.
static NEXT_MONITOR_ID: AtomicU64 = AtomicU64::new(1);

fn register_monitor(tree: Arc<ProcessTree>) -> u64 {
    let id = NEXT_MONITOR_ID.fetch_add(1, Ordering::Relaxed);
    MONITOR_TREES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((id, tree));
    id
}

fn unregister_monitor(id: u64) {
    let mut trees = MONITOR_TREES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    trees.retain(|(existing, _)| *existing != id);
}

/// Kill every live plugin monitor and anything it started.
///
/// Called from the SIGTERM handler and the panic hook, where no destructor
/// will run. Idempotent; safe when no monitor is armed.
pub fn terminate_all_monitors() {
    let trees = std::mem::take(
        &mut *MONITOR_TREES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for (_, tree) in trees {
        tree.terminate();
    }
}

/// Most monitors one session will arm.
///
/// A plugin document that names more is truncated at this count, with one
/// warning, so a runaway manifest cannot exhaust the process table.
pub const MAX_MONITORS: usize = 32;

/// Most bytes of a single monitor output line that are written to the log.
const MAX_LOG_LINE_BYTES: usize = 8 * 1024;

/// Owns the running tasks of a session's plugin monitors.
///
/// Dropping the supervisor aborts every task. Each task owns its child with
/// `kill_on_drop(true)`, so aborting the task also kills the process; [`stop`]
/// further waits for the aborts to finish, which is what a graceful shutdown
/// should call.
///
/// [`stop`]: PluginMonitorSupervisor::stop
#[derive(Debug)]
pub struct PluginMonitorSupervisor {
    monitors: Vec<MonitorTask>,
}

/// One armed monitor: its name, for logging, and the task that owns the child.
#[derive(Debug)]
struct MonitorTask {
    name: String,
    task: JoinHandle<()>,
}

/// One validated monitor definition, before it is spawned.
#[derive(Debug)]
struct MonitorSpec {
    name: String,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl PluginMonitorSupervisor {
    /// Arm every valid monitor in `monitors`, resolving relative `script`
    /// paths against `cwd`.
    ///
    /// Monitors are spawned with a null stdin and piped stdout/stderr. Their
    /// output is drained line by line into the log under the monitor's name:
    /// stdout at `info`, stderr at `warn`. Individual lines longer than
    /// 8 KiB are truncated.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `monitors` is neither an object nor an array, or
    /// when called outside a Tokio runtime. A single monitor that fails to
    /// spawn is logged and skipped instead of failing the whole call.
    pub fn start(monitors: &Value, cwd: &Path) -> Result<Self, String> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err("plugin monitors must be started from within a Tokio runtime".to_string());
        }

        let mut supervisor = Self {
            monitors: Vec::new(),
        };
        let mut over_cap = 0usize;

        for spec in monitor_specs(monitors, cwd)? {
            if supervisor.monitors.len() >= MAX_MONITORS {
                over_cap += 1;
                continue;
            }
            let name = spec.name.clone();
            match spawn_monitor(spec, cwd) {
                Ok(monitor) => supervisor.monitors.push(monitor),
                Err(error) => {
                    tracing::warn!(
                        monitor = %name,
                        %error,
                        "plugin monitor failed to start; skipping it"
                    );
                }
            }
        }

        if over_cap > 0 {
            tracing::warn!(
                max = MAX_MONITORS,
                skipped = over_cap,
                "plugin monitor limit reached; extra monitors were not started"
            );
        }

        Ok(supervisor)
    }

    /// Number of monitors whose task has not finished.
    ///
    /// A monitor that exited on its own no longer counts as running; its exit
    /// has already been logged.
    #[must_use]
    pub fn running(&self) -> usize {
        self.monitors
            .iter()
            .filter(|monitor| !monitor.task.is_finished())
            .count()
    }

    /// Abort and wait for every monitor.
    ///
    /// Idempotent: a second call, or a call after the monitors exited on
    /// their own, does nothing. Aborting a task drops its child, which
    /// `kill_on_drop(true)` turns into a `SIGKILL` on Unix.
    pub async fn stop(&mut self) {
        let monitors = std::mem::take(&mut self.monitors);
        for monitor in &monitors {
            monitor.task.abort();
        }
        for monitor in monitors {
            match monitor.task.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    tracing::warn!(
                        monitor = %monitor.name,
                        %error,
                        "plugin monitor task failed"
                    );
                }
            }
        }
    }
}

impl Drop for PluginMonitorSupervisor {
    /// Abort the monitor tasks that [`stop`](Self::stop) did not already take.
    ///
    /// The child is killed by `kill_on_drop` when the aborted task drops it.
    fn drop(&mut self) {
        for monitor in &self.monitors {
            monitor.task.abort();
        }
    }
}

/// Normalise either document shape into validated specs.
///
/// An entry that does not validate is warned about and skipped; only the
/// document's top-level shape can make this fail.
fn monitor_specs(monitors: &Value, cwd: &Path) -> Result<Vec<MonitorSpec>, String> {
    let mut specs = Vec::new();
    match monitors {
        Value::Null => {}
        Value::Object(entries) => {
            for (name, value) in entries {
                if name.is_empty() {
                    tracing::warn!("skipping plugin monitor with an empty name");
                    continue;
                }
                if let Some(spec) = parse_monitor(name, value, cwd) {
                    specs.push(spec);
                }
            }
        }
        Value::Array(entries) => {
            for (index, value) in entries.iter().enumerate() {
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    tracing::warn!(index, "skipping plugin monitor entry with no string `name`");
                    continue;
                };
                if name.is_empty() {
                    tracing::warn!(index, "skipping plugin monitor entry with an empty `name`");
                    continue;
                }
                if let Some(spec) = parse_monitor(name, value, cwd) {
                    specs.push(spec);
                }
            }
        }
        other => {
            return Err(format!(
                "plugin monitors must be a JSON object or array, found {}",
                json_kind(other)
            ));
        }
    }
    Ok(specs)
}

/// Validate one monitor definition.
///
/// Returns `None`, after logging why, for anything malformed. The caller keeps
/// going: a plugin's bad entry is its own monitor's problem.
fn parse_monitor(name: &str, value: &Value, cwd: &Path) -> Option<MonitorSpec> {
    let Some(entry) = value.as_object() else {
        tracing::warn!(
            monitor = %name,
            "skipping plugin monitor: definition must be an object"
        );
        return None;
    };

    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty());
    let script = entry
        .get("script")
        .and_then(Value::as_str)
        .filter(|script| !script.is_empty());

    let program = if let Some(command) = command {
        if script.is_some() {
            tracing::warn!(
                monitor = %name,
                "plugin monitor declares both `command` and `script`; using `command`"
            );
        }
        command.to_string()
    } else if let Some(script) = script {
        // A script is a path, not a name to look up on `PATH`.
        let script = Path::new(script);
        if script.is_absolute() {
            script.to_string_lossy().into_owned()
        } else {
            cwd.join(script).to_string_lossy().into_owned()
        }
    } else {
        tracing::warn!(
            monitor = %name,
            "skipping plugin monitor: no non-empty `command` or `script`"
        );
        return None;
    };

    let args = match entry.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => {
            let mut args = Vec::with_capacity(values.len());
            for value in values {
                let Some(arg) = value.as_str() else {
                    tracing::warn!(
                        monitor = %name,
                        "skipping plugin monitor: every `args` entry must be a string"
                    );
                    return None;
                };
                args.push(arg.to_string());
            }
            args
        }
        Some(_) => {
            tracing::warn!(
                monitor = %name,
                "skipping plugin monitor: `args` must be an array of strings"
            );
            return None;
        }
    };

    let env = match entry.get("env") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(values)) => {
            let mut env = Vec::with_capacity(values.len());
            for (key, value) in values {
                if key.is_empty() {
                    tracing::warn!(
                        monitor = %name,
                        "skipping plugin monitor: environment variable name is empty"
                    );
                    return None;
                }
                let Some(value) = value.as_str() else {
                    tracing::warn!(
                        monitor = %name,
                        variable = %key,
                        "skipping plugin monitor: environment values must be strings"
                    );
                    return None;
                };
                env.push((key.clone(), value.to_string()));
            }
            env
        }
        Some(_) => {
            tracing::warn!(
                monitor = %name,
                "skipping plugin monitor: `env` must be an object of strings"
            );
            return None;
        }
    };

    Some(MonitorSpec {
        name: name.to_string(),
        program,
        args,
        env,
    })
}

/// Spawn one monitor and the task that owns it.
fn spawn_monitor(spec: MonitorSpec, cwd: &Path) -> std::io::Result<MonitorTask> {
    let tree = Arc::new(ProcessTree::create(JobLimits::default())?);
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A task aborted at shutdown drops this child, and the drop kills it.
        .kill_on_drop(true);
    // Unix: own group. Windows: create suspended, so `attach_pid` runs before
    // the monitor executes and nothing it starts can escape the job.
    tree.prepare(command.as_std_mut());

    // The monitor is not a provider client. Remove the session's credentials
    // before layering the monitor's own environment on: an explicit entry is
    // the plugin asking for a variable, and wins over the removal.
    scrub_credentials(&mut command, &spec.env);
    command.envs(spec.env.iter().map(|(key, value)| (key, value)));

    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("the monitor exited before it could be registered"))?;
    if let Err(error) = tree.attach_pid(pid) {
        let _ = child.start_kill();
        return Err(std::io::Error::other(format!(
            "could not place the monitor in its process tree: {error}"
        )));
    }
    let id = register_monitor(Arc::clone(&tree));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let name = spec.name;
    let task = tokio::spawn(pump(child, id, tree, stdout, stderr, name.clone()));
    Ok(MonitorTask { name, task })
}

/// Remove this session's credential variables from `command`'s environment.
///
/// The exact names come from the same helper the shell and MCP tools use. The
/// reference also treats the whole `ANTHROPIC_*`, `CLAUDE_*`, and `OTEL_*`
/// families as host credentials, so those prefixes are added on top.
/// A name the monitor names explicitly in its own `env` is not removed.
fn scrub_credentials(command: &mut Command, explicit_env: &[(String, String)]) {
    let explicit: HashSet<&str> = explicit_env.iter().map(|(key, _)| key.as_str()).collect();

    let mut keys = coderex_core::sandbox::credential_scrub_keys();
    keys.extend(
        std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| is_credential_prefix(key)),
    );
    keys.sort_unstable();
    keys.dedup();

    for key in keys {
        if !explicit.contains(key.as_str()) {
            command.env_remove(key);
        }
    }
}

/// Whether `key` belongs to one of the credential families the host owns.
fn is_credential_prefix(key: &str) -> bool {
    ["ANTHROPIC_", "CLAUDE_", "OTEL_"]
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

/// Own the child for its whole lifetime, draining both output streams.
///
/// Waiting for the streams to reach EOF before `wait()` means the exit is
/// observed even when the monitor is chatty. Aborting this task drops the
/// child, and `kill_on_drop` kills (and the runtime reaps) the process.
async fn pump(
    mut child: Child,
    id: u64,
    tree: Arc<ProcessTree>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    name: String,
) {
    let stdout = drain_lines(stdout, &name, false);
    let stderr = drain_lines(stderr, &name, true);
    tokio::join!(stdout, stderr);

    match child.wait().await {
        Ok(status) => tracing::debug!(monitor = %name, %status, "plugin monitor exited"),
        Err(error) => {
            tracing::warn!(monitor = %name, %error, "could not wait for plugin monitor");
        }
    }
    // The monitor exited on its own. Its remaining descendants (if any) are
    // not ours to kill, so disarm before the tree is dropped.
    tree.disarm();
    unregister_monitor(id);
}

/// Drain one child stream, logging each line with the monitor's name.
///
/// Reading bytes rather than `lines()` keeps a monitor that emits invalid
/// UTF-8 from stranding the pipe; undecodable bytes are replaced.
async fn drain_lines<R>(stream: Option<R>, name: &str, is_stderr: bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(stream) = stream else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    continue;
                }
                let line = bounded_line(line);
                if is_stderr {
                    tracing::warn!(monitor = %name, line = %line, "plugin monitor stderr");
                } else {
                    tracing::info!(monitor = %name, line = %line, "plugin monitor stdout");
                }
            }
            Err(error) => {
                tracing::warn!(
                    monitor = %name,
                    %error,
                    "could not read plugin monitor output"
                );
                return;
            }
        }
    }
}

/// Truncate an over-long line at a character boundary for logging.
fn bounded_line(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_LOG_LINE_BYTES {
        return Cow::Borrowed(line);
    }
    let mut end = MAX_LOG_LINE_BYTES;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}... ({} bytes truncated)",
        &line[..end],
        line.len() - end
    ))
}

/// Name a JSON value for the top-level shape error.
const fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    /// Tests share one global PID registry, so a test that kills every
    /// monitor must not run concurrently with the others.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wait until `path` exists.
    ///
    /// Generous enough to survive a loaded CI machine: the assertion is about
    /// the monitor starting, not about scheduler latency.
    #[cfg(unix)]
    async fn wait_for_file(path: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !path.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until `path` contains `needle`, and return its contents.
    #[cfg(unix)]
    async fn wait_for_contents(path: &Path, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && contents.contains(needle)
            {
                return contents;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {needle:?} in {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn starts_an_echoing_monitor_and_reports_it_running() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let monitors = json!({
            "echoer": {
                // Absolute paths: another test in this binary replaces PATH.
                "command": "/bin/sh",
                "args": ["-c", "echo monitor-ready; exec /bin/sleep 60"]
            }
        });

        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 1);

        supervisor.stop().await;
        assert_eq!(supervisor.running(), 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn resolves_a_relative_script_against_the_working_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let _guard = serial();

        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("monitor.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'script-ran\\n' > marker\nexec sleep 60\n",
        )
        .expect("write monitor script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make monitor executable");

        let monitors = json!({ "scripted": { "script": "monitor.sh" } });
        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 1);

        wait_for_file(&dir.path().join("marker")).await;
        assert_eq!(supervisor.running(), 1);
        supervisor.stop().await;
    }

    #[tokio::test]
    async fn skips_invalid_entries() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let monitors = json!({
            "not-an-object": "nope",
            "no-program": { "args": ["-x"] },
            "empty-command": { "command": "" },
            "bad-args": { "command": "true", "args": "not-an-array" },
            "bad-arg-entry": { "command": "true", "args": [7] },
            "bad-env": { "command": "true", "env": ["nope"] },
            "bad-env-value": { "command": "true", "env": { "KEY": 7 } },
            "": { "command": "true" }
        });

        let supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 0);
    }

    #[tokio::test]
    async fn array_entries_must_carry_a_name() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let monitors = json!([
            { "command": "true" },
            { "name": "", "command": "true" },
            { "name": 7, "command": "true" }
        ]);

        let supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 0);
    }

    #[tokio::test]
    async fn rejects_a_document_that_is_not_an_object_or_array() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let error = PluginMonitorSupervisor::start(&json!("nope"), dir.path())
            .expect_err("a string is not a monitor document");
        assert!(
            error.contains("object or array"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn stop_kills_the_child() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let monitors = json!({
            "sleeper": {
                "command": "/bin/sh",
                "args": [
                    "-c",
                    "echo $$ > \"$1\"; exec /bin/sleep 60",
                    "sh",
                    pid_file.to_string_lossy()
                ]
            }
        });

        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        wait_for_file(&pid_file).await;
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("read pid file")
            .trim()
            .parse()
            .expect("parse pid");

        supervisor.stop().await;
        assert_eq!(supervisor.running(), 0);

        // The child was killed. `kill -0` also succeeds for a zombie, so poll
        // while the runtime's reaper finishes with it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -0 {pid} 2>/dev/null"))
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "child {pid} survived stop"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(unsafe_code)] // SAFETY: the environment mutation is serialised by ENV_LOCK.
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn removes_credential_variables_from_the_child_environment() {
        const SCRUBBED: [(&str, &str); 4] = [
            ("ANTHROPIC_API_KEY", "test-anthropic-key"),
            ("ANTHROPIC_CUSTOM_SECRET", "test-anthropic-custom"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "test-claude-token"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "Authorization=test-otel"),
        ];
        const KEPT: (&str, &str) = ("CODEREX_MONITOR_TEST_KEEP", "kept");

        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join("env.txt");

        let guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Remember the previous values so the test leaves the process as it
        // found it.
        let previous: Vec<(&str, Option<std::ffi::OsString>)> = SCRUBBED
            .iter()
            .chain(std::iter::once(&KEPT))
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();

        // SAFETY: `ENV_LOCK` is held for the whole test, and this module is
        // the only one that mutates these names.
        unsafe {
            for (key, value) in SCRUBBED {
                std::env::set_var(key, value);
            }
            std::env::set_var(KEPT.0, KEPT.1);
        }

        let monitors = json!({
            "env-dumper": {
                "command": "/bin/sh",
                "args": [
                    "-c",
                    "env > \"$1\"",
                    "sh",
                    env_file.to_string_lossy()
                ],
                "env": {
                    "CODEREX_MONITOR_TEST_EXPLICIT": "explicit",
                    "ANTHROPIC_EXPLICIT_OVERRIDE": "explicit"
                }
            }
        });

        // Spawning snapshots the environment; restore the parent as soon as
        // the child exists.
        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        // SAFETY: as above; `ENV_LOCK` is still held.
        unsafe {
            for (key, value) in previous {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }

        // The child's environment was snapshotted at spawn, so the lock is no
        // longer needed and must not be held across the waits below.
        drop(guard);

        let contents = wait_for_contents(&env_file, "CODEREX_MONITOR_TEST_EXPLICIT=explicit").await;

        for (key, _) in SCRUBBED {
            let leaked = contents
                .lines()
                .any(|line| line.starts_with(&format!("{key}=")));
            assert!(!leaked, "{key} leaked into the monitor environment");
        }
        assert!(
            contents.contains("CODEREX_MONITOR_TEST_KEEP=kept"),
            "non-credential variables must be inherited"
        );
        assert!(
            contents.contains("ANTHROPIC_EXPLICIT_OVERRIDE=explicit"),
            "an explicitly named environment variable must win over scrubbing"
        );

        supervisor.stop().await;
    }

    /// A signal handler cannot run destructors, so the registry is the only
    /// cleanup. `terminate_all_monitors` must kill a child without `stop()`.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn terminate_all_kills_monitors_without_drop() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        let document = serde_json::json!({
            // Absolute path: another test in this binary replaces `PATH`, and a
            // PATH lookup here would make this test depend on test order.
            "sleeper": { "command": "/bin/sh", "args": ["-c", "sleep 60"] }
        });
        let supervisor = PluginMonitorSupervisor::start(&document, dir.path()).unwrap();
        assert_eq!(supervisor.running(), 1);

        terminate_all_monitors();

        let mut running = supervisor.running();
        for _ in 0..100 {
            if running == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            running = supervisor.running();
        }
        assert_eq!(running, 0, "the monitor survived terminate_all_monitors");
    }
}
