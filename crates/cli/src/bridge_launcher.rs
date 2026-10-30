//! Production entry point for the bridge worker.
//!
//! `coderex bridge` resolves a [`BridgeLaunchConfig`] from CLI flags and
//! environment variables, builds the configured transport through
//! `coderex_bridge::transport::default_factory()`, constructs a [`Bridge`],
//! and runs [`Bridge::serve_transport`] until Ctrl+C or SIGTERM.
//!
//! The transport is injectable ([`run_with_transport`]) so tests can drive a
//! real bridge over an in-process transport without a network.

use anyhow::{Context, Result};
use coderex_bridge::archive::{ProcessRegistry, UsageStats};
use coderex_bridge::heartbeat::HeartbeatConfig;
use coderex_bridge::orchestrator::{Bridge, BridgeConfig, BridgeEvent, BridgeStopHandle};
use coderex_bridge::permission::PermissionResponse;
use coderex_bridge::poller::{
    ClaimOutcome, ControlPlaneClient, DEFAULT_CLAIM_PATH_TEMPLATE, HttpControlPlaneClient,
};
use coderex_bridge::transport::{Transport, TransportConfig, default_factory};
use coderex_core::process_tree::{JobLimits, ProcessTree};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::cli::BridgeArgs;

/// Environment variable naming the control-plane HTTP API.
pub const ENV_CONTROL_PLANE_URL: &str = "CODEREX_BRIDGE_CONTROL_PLANE_URL";
/// Environment variable naming this environment.
pub const ENV_ENV_ID: &str = "CODEREX_BRIDGE_ENV_ID";
/// Environment variable naming the repository root.
pub const ENV_REPO_ROOT: &str = "CODEREX_BRIDGE_REPO_ROOT";
/// Environment variable naming the worktree base directory.
pub const ENV_WORKTREE_BASE: &str = "CODEREX_BRIDGE_WORKTREE_BASE";
/// Environment variable naming the session base directory.
pub const ENV_SESSION_BASE: &str = "CODEREX_BRIDGE_SESSION_BASE";
/// Environment variable naming the archive base directory.
pub const ENV_ARCHIVE_BASE: &str = "CODEREX_BRIDGE_ARCHIVE_BASE";
/// Environment variable selecting the transport kind.
pub const ENV_TRANSPORT: &str = "CODEREX_BRIDGE_TRANSPORT";
/// Environment variable naming the transport endpoint. The historical spelling
/// is a `ws://host:port` URL.
pub const ENV_TRANSPORT_URL: &str = "CODEREX_BRIDGE_URL";
/// Environment variable overriding the transport connection ID.
pub const ENV_CONNECTION_ID: &str = "CODEREX_BRIDGE_CONNECTION_ID";
/// Environment variable carrying the bearer token. Prefer a flag or an
/// inherited environment over a shell history entry.
pub const ENV_AUTH_TOKEN: &str = "CODEREX_BRIDGE_TOKEN";
/// Environment variable setting the assignment poll interval, in seconds.
pub const ENV_POLL_INTERVAL_SECS: &str = "CODEREX_BRIDGE_POLL_INTERVAL_SECS";
/// Environment variable overriding the assignment claim path template.
pub const ENV_CLAIM_PATH: &str = "CODEREX_BRIDGE_CLAIM_PATH";
/// Environment variable bounding how long a child's permission request may
/// wait for an answer, in seconds. `0` disables the bound.
pub const ENV_APPROVAL_TIMEOUT_SECS: &str = "CODEREX_BRIDGE_APPROVAL_TIMEOUT_SECS";

/// Default transport kind: the real gateway transport.
pub const DEFAULT_TRANSPORT: &str = "websocket";
/// Default maximum concurrent sessions.
pub const DEFAULT_MAX_CONCURRENT: usize = 5;
/// Default interval between control-plane assignment polls.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Default bound on a child's permission request, in seconds.
///
/// Five minutes is long enough for a person to notice and answer an approval
/// on a remote session, and short enough that a gateway that dropped the
/// question does not leave the child blocked forever.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 300;
/// How many times registration is attempted before the worker exits.
pub const REGISTRATION_ATTEMPTS: usize = 5;

/// How long a session child gets to exit after a graceful termination request
/// before the worker kills it.
pub const SESSION_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How long the worker waits for its session tasks to finish after cancelling
/// them before aborting the remaining tasks.
pub const SESSION_TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(6);

/// A cancellation signal handed to a running session.
///
/// Each session task owns one; the worker cancels it on shutdown. The default
/// runner turns that into `SIGTERM` (Unix) or process termination (Windows),
/// then waits for the child to exit before killing it.
#[derive(Debug, Clone)]
pub struct SessionStop {
    cancelled: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl SessionStop {
    /// Create a session stop signal that has not been cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Ask the session to stop.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // As with `BridgeStopHandle`, a permit left behind means a cancel
        // requested before the runner starts waiting is not lost.
        self.notify.notify_one();
    }

    /// Whether a stop has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Wait until a stop is requested.
    pub async fn wait(&self) {
        while !self.is_cancelled() {
            self.notify.notified().await;
        }
    }
}

impl Default for SessionStop {
    fn default() -> Self {
        Self::new()
    }
}

/// One line of a session's stdout or stderr, bound for the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOutputLine {
    /// Session the line came from.
    pub session_id: String,
    /// `"stdout"` or `"stderr"`.
    pub stream: &'static str,
    /// The line, without its trailing newline.
    pub line: String,
}

/// One tool-approval request emitted by a session's child process.
///
/// This is the launcher's view of a `control_request`/`can_use_tool` frame:
/// the child has stopped and will not continue until it receives an answer on
/// stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildPermissionRequest {
    /// Session whose child asked.
    pub session_id: String,
    /// The id the child chose; the answer must echo it.
    pub request_id: String,
    /// Tool that wants to run.
    pub tool_name: String,
    /// The tool's input, unmodified.
    pub input: Value,
    /// Why the tool needs approval.
    pub reason: String,
    /// Whether the child offered a session-wide answer.
    pub allow_session: bool,
}

/// The launcher's answer to a [`ChildPermissionRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildPermissionDecision {
    /// Request being answered.
    pub request_id: String,
    /// Whether the tool may run.
    pub allow: bool,
    /// Whether the approval covers the rest of the session.
    pub allow_session: bool,
    /// Detail written into the `control_response`.
    pub message: String,
}

/// The receiving half of a session's decision channel.
///
/// Stored in a slot because [`SessionRunner::run`] receives a shared reference
/// to the context; the process runner takes the receiver once, at the start of
/// the run.
pub type PermissionDecisionSlot =
    Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<ChildPermissionDecision>>>>;

/// Everything a [`SessionRunner`] needs for one session.
#[derive(Debug, Clone)]
pub struct SessionContext<'a> {
    /// Session ID assigned by the control plane.
    pub session_id: &'a str,
    /// Prompt to run.
    pub prompt: &'a str,
    /// Directory the child runs in.
    pub work_dir: &'a Path,
    /// Directory where the runner writes the transcript and stderr log.
    pub session_dir: &'a Path,
    /// Where the runner sends each output line as the child produces it.
    ///
    /// `None` when the caller does not want streaming (a test, or a runner
    /// used outside the worker loop).
    pub output: Option<tokio::sync::mpsc::UnboundedSender<SessionOutputLine>>,
    /// Where the runner sends each `can_use_tool` request the child emits.
    ///
    /// `None` when nobody is listening; the child then keeps waiting, which is
    /// the pre-existing behaviour for a runner used outside the worker loop.
    pub permission_requests: Option<tokio::sync::mpsc::UnboundedSender<ChildPermissionRequest>>,
    /// Answers the launcher routed to this session.
    pub permission_decisions: Option<PermissionDecisionSlot>,
    /// The bridge's registry of live session processes.
    ///
    /// The runner registers the child it spawns here, so the bridge's cleanup
    /// terminates the process tree it owns rather than an empty map. `None`
    /// when the runner is driven outside the worker loop (a test, or a
    /// standalone runner), where there is no cleanup to register for.
    pub process_registry: Option<ProcessRegistry>,
}

/// What a successful session produced.
#[derive(Debug, Clone, Default)]
pub struct SessionOutput {
    /// Complete child stdout transcript, one JSON frame per line.
    pub transcript: String,
    /// Usage reported by the turn's final `result` frame.
    pub usage: UsageStats,
}

/// Runs a spawned assignment's prompt in its working directory.
///
/// The default implementation starts this executable headless. Tests inject a
/// fake so a worker can be exercised without running an agent turn.
#[async_trait::async_trait]
pub trait SessionRunner: Send + Sync {
    /// Run one session.
    ///
    /// Returns the captured transcript and usage when the session finished
    /// successfully, or a short error (including the exit status when one is
    /// available) otherwise. `stop` is cancelled when the bridge shuts down;
    /// a runner should terminate its child gracefully in response.
    async fn run(
        &self,
        context: &SessionContext<'_>,
        stop: SessionStop,
    ) -> Result<SessionOutput, String>;
}

/// Runs a session by invoking this executable in headless, streaming mode.
#[derive(Debug, Clone)]
pub struct ProcessSessionRunner {
    /// Grace period between the termination request and the kill.
    grace: Duration,
}

impl Default for ProcessSessionRunner {
    fn default() -> Self {
        Self {
            grace: shutdown_grace(),
        }
    }
}

impl ProcessSessionRunner {
    /// Create a runner with the default shutdown grace period.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a runner with an explicit shutdown grace period.
    #[must_use]
    pub const fn with_grace(grace: Duration) -> Self {
        Self { grace }
    }
}

/// Build the headless invocation for a session.
///
/// `--output-format stream-json` makes the child write one JSON object per
/// line, ending each turn with a single `result` frame. The prompt is passed
/// as one positional argument after `--`, so it reaches the child verbatim and
/// no shell ever sees it.
fn session_command(prompt: &str, work_dir: &Path) -> Result<Command, String> {
    let executable = std::env::current_exe()
        .map_err(|err| format!("cannot locate the coderex executable: {err}"))?;
    let mut command = Command::new(executable);
    command
        .arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--")
        .arg(prompt)
        .current_dir(work_dir)
        // The child's `control_response` answers arrive here. The prompt is
        // still positional, so stdin carries the control channel only.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping the future (shutdown, panic) kills the child instead of
        // leaving an orphan agent turn behind. Normal shutdown asks NICELY
        // first; see `terminate_child`.
        .kill_on_drop(true);
    Ok(command)
}

#[async_trait::async_trait]
impl SessionRunner for ProcessSessionRunner {
    async fn run(
        &self,
        context: &SessionContext<'_>,
        stop: SessionStop,
    ) -> Result<SessionOutput, String> {
        let mut command = session_command(context.prompt, context.work_dir)?;
        // Spawn inside a process tree and register it with the bridge, so the
        // cleanup that runs when the session ends terminates the child (and
        // anything it started) instead of an empty map.
        let (child, _tree) = spawn_registered_child(
            &mut command,
            context.session_id,
            context.process_registry.as_ref(),
        )?;
        // A shared reference cannot move the receiver out, so the launcher
        // leaves it in the slot and the one runner that owns the child takes
        // it here.
        let decisions = context
            .permission_decisions
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|mut slot| slot.take()));
        run_child_streaming(
            child,
            context.session_id,
            context.session_dir,
            context.output.clone(),
            context.permission_requests.clone(),
            decisions,
            stop,
            self.grace,
            // `spawn_registered_child` put the child in its own process group
            // (`ProcessTree::prepare`), so a stop can reach its children too.
            true,
        )
        .await
    }
}

/// Spawn a session child inside a process tree and register it for cleanup.
///
/// The tree is created and armed before the spawn, so on Windows the child is
/// created suspended and joined to the Job Object before it runs. When a
/// registry is supplied the tree is registered under `session_id`, which is
/// what lets [`coderex_bridge::orchestrator::Bridge::complete_session`]'s
/// cleanup terminate the whole tree. Without a registry (a standalone runner)
/// the child is spawned normally.
fn spawn_registered_child(
    command: &mut Command,
    session_id: &str,
    registry: Option<&ProcessRegistry>,
) -> Result<(Child, Option<Arc<ProcessTree>>), String> {
    let Some(registry) = registry else {
        let child = command
            .spawn()
            .map_err(|err| format!("failed to start the session process: {err}"))?;
        return Ok((child, None));
    };

    let tree = Arc::new(
        ProcessTree::create(JobLimits::default())
            .map_err(|err| format!("cannot create a process tree for the session: {err}"))?,
    );
    tree.prepare(command.as_std_mut());
    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to start the session process: {err}"))?;
    if let Some(pid) = child.id()
        && let Err(err) = tree.attach_pid(pid)
    {
        // The child is not in the tree it was promised; it must not run
        // outside the job that would have owned it.
        let _ = child.start_kill();
        return Err(format!(
            "cannot attach the session process to its tree: {err}"
        ));
    }
    registry.track_tree(session_id, Arc::clone(&tree));
    Ok((child, Some(tree)))
}

/// Captured stdout for one session.
#[derive(Debug, Default)]
struct StreamCapture {
    transcript: String,
    /// The turn's usage: the final `result` frame's totals when it carries
    /// them, otherwise the sum of every assistant frame's own usage.
    usage: UsageStats,
    /// Usage summed from the assistant frames, kept separately so the result
    /// frame can override it and tests can see both.
    streamed_usage: UsageStats,
    /// Detail from the final `result` frame when the turn reported an error.
    result_error: Option<String>,
}

/// How long a stopped session gets before it is killed.
///
/// `CODEREX_BRIDGE_SHUTDOWN_GRACE_SECS` overrides the default; a session whose
/// child has real cleanup to do (writing a transcript, stopping a database)
/// needs more than five seconds, and the right value depends on that child.
#[must_use]
fn shutdown_grace() -> Duration {
    shutdown_grace_from(
        std::env::var("CODEREX_BRIDGE_SHUTDOWN_GRACE_SECS")
            .ok()
            .as_deref(),
    )
}

/// The override rule, pure so a test does not have to mutate the process
/// environment (which the workspace forbids outside `unsafe`).
#[must_use]
fn shutdown_grace_from(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(SESSION_SHUTDOWN_GRACE, Duration::from_secs)
}

/// Run a child with piped stdout/stderr, streaming both concurrently.
///
/// The two pipes are drained by separate tasks, so a full stderr pipe cannot
/// block stdout (or the child). Stdout lines form the transcript and the final
/// `result` frame supplies usage. On a stop request the child is asked to exit
/// gracefully, given `grace`, and killed if it does not.
async fn run_child_streaming(
    mut child: Child,
    session_id: &str,
    session_dir: &Path,
    output: Option<tokio::sync::mpsc::UnboundedSender<SessionOutputLine>>,
    permission_requests: Option<tokio::sync::mpsc::UnboundedSender<ChildPermissionRequest>>,
    decisions: Option<tokio::sync::mpsc::UnboundedReceiver<ChildPermissionDecision>>,
    stop: SessionStop,
    grace: Duration,
    // Whether the child leads its own process group, in which case the
    // graceful stop also reaches the session's own children.
    group_leader: bool,
) -> Result<SessionOutput, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "session process has no stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "session process has no stderr pipe".to_string())?;
    let mut child_stdin = child.stdin.take();

    let stdout_session = session_id.to_string();
    let stderr_session = session_id.to_string();
    let stdout_output = output.clone();
    let stderr_output = output;
    let stdout_task = tokio::spawn(read_stdout(
        stdout,
        stdout_session,
        stdout_output,
        permission_requests,
    ));
    let stderr_task = tokio::spawn(read_stderr(stderr, stderr_session, stderr_output));

    // A runner with no launcher still needs a channel to select on, or the
    // select has to special-case its absence. Keeping a sender alive means
    // this receiver never reports "all senders dropped" and spins.
    let (keepalive_tx, keepalive_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut decisions = decisions.unwrap_or(keepalive_rx);
    let _keepalive = keepalive_tx;
    let mut decision_channel_open = true;

    let (status, cancelled) = loop {
        tokio::select! {
            status = child.wait() => break (
                status.map_err(|err| format!("failed to wait for the session process: {err}"))?,
                false,
            ),
            () = stop.wait() => break (
                terminate_child(&mut child, grace, group_leader).await?,
                true,
            ),
            decision = decisions.recv(), if decision_channel_open => {
                match decision {
                    Some(decision) => {
                        write_child_decision(&mut child_stdin, decision).await;
                    }
                    // The launcher dropped every sender; stop polling rather
                    // than spin on a permanently ready `None`.
                    None => decision_channel_open = false,
                }
            }
        }
    };

    let capture = stdout_task
        .await
        .map_err(|err| format!("session stdout reader failed: {err}"))??;
    let stderr_text = stderr_task
        .await
        .map_err(|err| format!("session stderr reader failed: {err}"))??;

    // The transcript belongs to the session directory even when the turn
    // failed or the worker is shutting down; keep the partial stream.
    write_session_file(session_dir, "transcript.jsonl", &capture.transcript);
    write_session_file(session_dir, "stderr.log", &stderr_text);

    if cancelled {
        return Err("session terminated during bridge shutdown".to_string());
    }
    if let Some(error) = capture.result_error {
        return Err(error);
    }
    if !status.success() {
        let status = exit_status_label(status);
        let detail: String = stderr_text.trim().chars().take(200).collect();
        return Err(if detail.is_empty() {
            status
        } else {
            format!("{status}: {detail}")
        });
    }

    Ok(SessionOutput {
        transcript: capture.transcript,
        usage: capture.usage,
    })
}

/// Write one decision to the child as a `control_response` line.
///
/// A broken pipe is not an error: the child may have exited between asking
/// and being answered. Logging it keeps the session's reader alive and lets
/// the exit status explain what happened.
async fn write_child_decision(
    stdin: &mut Option<tokio::process::ChildStdin>,
    decision: ChildPermissionDecision,
) {
    let Some(stdin) = stdin.as_mut() else {
        tracing::warn!(
            request_id = %decision.request_id,
            "session child has no stdin pipe; dropping permission decision"
        );
        return;
    };

    let frame = if decision.allow {
        let mut frame = coderex_events::protocol::ControlResponseFrame::allow(
            &decision.request_id,
            &decision.message,
        );
        if decision.allow_session {
            frame = frame.with_allow_session(true);
        }
        frame
    } else {
        coderex_events::protocol::ControlResponseFrame::deny(
            &decision.request_id,
            &decision.message,
        )
    };
    let line = frame.to_line();

    if let Err(error) = stdin.write_all(line.as_bytes()).await {
        tracing::warn!(
            request_id = %decision.request_id,
            error = %error,
            "could not write a control response to the session child"
        );
        return;
    }
    if let Err(error) = stdin.flush().await {
        tracing::warn!(
            request_id = %decision.request_id,
            error = %error,
            "could not flush a control response to the session child"
        );
    }
}

/// Read stdout line by line, forwarding it to tracing, the transport, and the
/// captured transcript.
///
/// A `control_request` line is also handed to the launcher loop: the child is
/// waiting on the answer, and the transcript alone does not unblock it.
async fn read_stdout<R>(
    reader: R,
    session_id: String,
    output: Option<tokio::sync::mpsc::UnboundedSender<SessionOutputLine>>,
    permission_requests: Option<tokio::sync::mpsc::UnboundedSender<ChildPermissionRequest>>,
) -> Result<StreamCapture, String>
where
    R: AsyncRead + Unpin,
{
    let mut capture = StreamCapture::default();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|err| format!("failed to read session stdout: {err}"))?
    {
        tracing::debug!(session_id = %session_id, stream = "stdout", "{}", line);
        forward_output(output.as_ref(), &session_id, "stdout", &line);
        capture.transcript.push_str(&line);
        capture.transcript.push('\n');

        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                match coderex_events::protocol::ControlRequestFrame::parse_line(&line) {
                    Ok(request) => {
                        if let Some(sender) = permission_requests.as_ref() {
                            let _ = sender.send(ChildPermissionRequest {
                                session_id: session_id.clone(),
                                request_id: request.request_id,
                                tool_name: request.request.tool_name,
                                input: request.request.input,
                                reason: request.request.reason,
                                allow_session: request.request.allow_session,
                            });
                        }
                    }
                    // One malformed line must not take down the reader: the
                    // child is still waiting for an answer it will not get,
                    // but the rest of the transcript is still useful.
                    Err(error) => tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "ignoring a malformed control_request from the session child"
                    ),
                }
                continue;
            }
            // An assistant frame carries its own usage; the turn's total is
            // the sum of them unless the result frame reports one.
            Some("assistant") => {
                if let Some(usage) = frame
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    capture.streamed_usage =
                        add_usage(capture.streamed_usage, usage_from_usage(usage));
                }
                continue;
            }
            Some("result") => {}
            _ => continue,
        }

        // The final `result` frame may carry the turn's totals, or it may not
        // carry usage at all (an older child, or a provider that reports per
        // message). The assistant frames' usage was accumulated above, so a
        // session that never emits totals still reports what it spent; the
        // result frame wins when it does carry them, because its numbers are
        // the provider's own totals rather than our sum.
        let from_result = usage_from_result(&frame);
        // Copied out, not moved: `streamed_usage` keeps accumulating.
        let streamed_so_far = capture.streamed_usage;
        capture.usage = if from_result.input_tokens == 0
            && from_result.output_tokens == 0
            && from_result.cache_read_tokens == 0
            && from_result.cache_creation_tokens == 0
        {
            streamed_so_far
        } else {
            from_result
        };
        if frame
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            capture.result_error = Some(result_error_message(&frame));
        }
    }

    Ok(capture)
}

/// Read stderr line by line, forwarding it to tracing and the transport, and
/// keeping the text.
async fn read_stderr<R>(
    reader: R,
    session_id: String,
    output: Option<tokio::sync::mpsc::UnboundedSender<SessionOutputLine>>,
) -> Result<String, String>
where
    R: AsyncRead + Unpin,
{
    let mut text = String::new();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|err| format!("failed to read session stderr: {err}"))?
    {
        tracing::warn!(session_id = %session_id, stream = "stderr", "{}", line);
        forward_output(output.as_ref(), &session_id, "stderr", &line);
        text.push_str(&line);
        text.push('\n');
    }

    Ok(text)
}

/// Hand one line to the worker's output channel, if one was supplied.
///
/// A closed channel means the worker is gone; the local transcript is still
/// written, so this is not an error.
fn forward_output(
    output: Option<&tokio::sync::mpsc::UnboundedSender<SessionOutputLine>>,
    session_id: &str,
    stream: &'static str,
    line: &str,
) {
    if let Some(output) = output {
        let _ = output.send(SessionOutputLine {
            session_id: session_id.to_string(),
            stream,
            line: line.to_string(),
        });
    }
}

/// Add two usage reports, field by field.
fn add_usage(total: UsageStats, add: UsageStats) -> UsageStats {
    UsageStats {
        input_tokens: total.input_tokens.saturating_add(add.input_tokens),
        output_tokens: total.output_tokens.saturating_add(add.output_tokens),
        cache_read_tokens: total
            .cache_read_tokens
            .saturating_add(add.cache_read_tokens),
        cache_creation_tokens: total
            .cache_creation_tokens
            .saturating_add(add.cache_creation_tokens),
        cost_usd: total.cost_usd + add.cost_usd,
    }
}

/// Read a usage object in either spelling.
fn usage_from_usage(usage: &Value) -> UsageStats {
    let tokens = |keys: &[&str]| -> usize {
        usize::try_from(
            keys.iter()
                .find_map(|key| usage.get(*key))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(usize::MAX)
    };
    UsageStats {
        input_tokens: tokens(&["input_tokens"]),
        output_tokens: tokens(&["output_tokens"]),
        cache_read_tokens: tokens(&["cache_read_input_tokens", "cache_read_tokens"]),
        cache_creation_tokens: tokens(&["cache_creation_input_tokens", "cache_creation_tokens"]),
        cost_usd: usage.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
    }
}

/// Extract usage from a stream-json `result` frame.
///
/// The protocol preserves provider fields it does not model, so both the
/// Anthropic spelling (`cache_read_input_tokens`) and the shorter archive
/// spelling (`cache_read_tokens`) are accepted.
fn usage_from_result(frame: &Value) -> UsageStats {
    let usage = frame.get("usage");
    let tokens = |keys: &[&str]| -> usize {
        usize::try_from(
            usage
                .and_then(|usage| keys.iter().find_map(|key| usage.get(*key)))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(usize::MAX)
    };

    UsageStats {
        input_tokens: tokens(&["input_tokens"]),
        output_tokens: tokens(&["output_tokens"]),
        cache_read_tokens: tokens(&["cache_read_input_tokens", "cache_read_tokens"]),
        cache_creation_tokens: tokens(&["cache_creation_input_tokens", "cache_creation_tokens"]),
        cost_usd: frame
            .get("total_cost_usd")
            .or_else(|| frame.get("cost_usd"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    }
}

/// Build the failure detail from an error `result` frame.
fn result_error_message(frame: &Value) -> String {
    let subtype = frame
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let detail = frame
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| frame.get("result").and_then(Value::as_str))
        .unwrap_or("");
    if detail.trim().is_empty() {
        format!("session reported {subtype}")
    } else {
        format!("session reported {subtype}: {}", detail.trim())
    }
}

/// Ask the child to exit, wait at most `grace`, then kill it.
async fn terminate_child(
    child: &mut Child,
    grace: Duration,
    // Only read on Unix; Windows has no signals.
    #[cfg_attr(not(unix), allow(unused_variables))] group_leader: bool,
) -> Result<ExitStatus, String> {
    // The child may have exited between the stop signal and this point.
    if let Some(status) = child
        .try_wait()
        .map_err(|err| format!("failed to inspect the session process: {err}"))?
    {
        return Ok(status);
    }

    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // The workspace denies `unsafe`, so this goes through the POSIX
            // `kill` utility rather than `libc::kill`.
            //
            // A session spawned through `spawn_registered_child` is its own
            // group leader (`ProcessTree` calls `process_group(0)`), so its
            // group is signalled and the session's own children get the
            // graceful request too. The caller says whether that is the case:
            // a pid that is merely *some* group's id would signal an unrelated
            // group, and guessing by trying the group first did exactly that
            // in a test harness.
            signal(pid, group_leader).await;
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no SIGTERM; TerminateProcess is the platform's
        // termination request.
        let _ = child.start_kill();
    }

    if let Ok(status) = tokio::time::timeout(grace, child.wait()).await {
        status.map_err(|err| format!("failed to wait for the session process: {err}"))
    } else {
        let _ = child.start_kill();
        child
            .wait()
            .await
            .map_err(|err| format!("failed to kill the session process: {err}"))
    }
}

/// `kill -TERM` the process, or the process group it leads when `group` is
/// set. Returns whether the signal was delivered.
#[cfg(unix)]
async fn signal(pid: u32, group: bool) -> bool {
    let target = if group {
        format!("-{pid}")
    } else {
        pid.to_string()
    };
    match Command::new("kill")
        .arg("-TERM")
        .arg(&target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            tracing::debug!(
                pid,
                group,
                status = ?status.code(),
                "SIGTERM was not delivered"
            );
            false
        }
        Err(err) => {
            tracing::warn!(pid, error = %err, "could not run kill to terminate the session process");
            false
        }
    }
}

fn exit_status_label(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_string(),
        |code| format!("exit code {code}"),
    )
}

/// Write a session artifact, logging rather than failing when it cannot be
/// written (the turn's outcome is more important than the local copy).
fn write_session_file(session_dir: &Path, name: &str, contents: &str) {
    if let Err(err) = std::fs::create_dir_all(session_dir) {
        tracing::warn!(
            dir = %session_dir.display(),
            error = %err,
            "cannot create session directory for {name}"
        );
        return;
    }
    if let Err(err) = std::fs::write(session_dir.join(name), contents) {
        tracing::warn!(
            dir = %session_dir.display(),
            file = name,
            error = %err,
            "cannot write session artifact"
        );
    }
}

/// A session that finished running, sent from its task back to the worker.
#[derive(Debug)]
struct SessionCompletion {
    session_id: String,
    outcome: Result<SessionOutput, String>,
}

/// A session that was spawned and is ready to be run.
#[derive(Debug)]
struct SpawnedSession {
    session_id: String,
    work_dir: PathBuf,
    session_dir: PathBuf,
    prompt: String,
}

/// A running session task and the signal that asks it to stop.
#[derive(Debug)]
struct RunningSession {
    handle: tokio::task::JoinHandle<()>,
    stop: SessionStop,
    /// Keeps the session's decision channel open while it runs.
    ///
    /// Per-request clones live in the pending map; this one exists so a
    /// session with no outstanding request still has a live sender and its
    /// runner never sees the channel close.
    permission_tx: tokio::sync::mpsc::UnboundedSender<ChildPermissionDecision>,
}

/// A child permission request waiting for the person's answer.
#[derive(Debug)]
struct PendingChildPermission {
    /// Session whose child is blocked.
    session_id: String,
    /// Where the child's answer goes.
    decisions: tokio::sync::mpsc::UnboundedSender<ChildPermissionDecision>,
    /// Dropped when the request resolves. The bounded wait owns the receiver,
    /// so dropping this ends the wait without a timeout firing.
    _answered: tokio::sync::oneshot::Sender<()>,
}

/// Tracks the child permission requests the launcher has not answered yet.
///
/// It owns the bridge-side registration (`request_permission_with_id`) and the
/// session-state edges in and out of `AwaitingPermission`, so the run loop and
/// the timeout tests drive exactly the same code.
#[derive(Debug)]
struct PermissionTracker {
    /// Requests registered with the bridge and still waiting.
    pending: HashMap<String, PendingChildPermission>,
    /// How many requests each session has outstanding. The session stays in
    /// `AwaitingPermission` until the last one resolves.
    session_counts: HashMap<String, usize>,
    /// Bound on each wait; `None` preserves the unbounded historical behaviour.
    timeout: Option<Duration>,
    /// Deadline notifications: a request id whose bounded wait expired.
    timeout_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl PermissionTracker {
    fn new(
        timeout: Option<Duration>,
        timeout_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            pending: HashMap::new(),
            session_counts: HashMap::new(),
            timeout,
            timeout_tx,
        }
    }

    /// Number of requests still waiting for an answer.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }

    /// Register a child request with the bridge and start its bounded wait.
    async fn register(
        &mut self,
        bridge: &mut Bridge,
        running: &HashMap<String, RunningSession>,
        request: ChildPermissionRequest,
    ) {
        if self.pending.contains_key(&request.request_id) {
            tracing::warn!(
                session_id = %request.session_id,
                request_id = %request.request_id,
                "ignoring a child permission request id that is already pending"
            );
            return;
        }

        let Some(decisions) = running
            .get(&request.session_id)
            .map(|session| session.permission_tx.clone())
        else {
            // The session finished while its last frame was in the pipe;
            // nobody can act on the answer, and there is no child left to
            // receive it.
            tracing::warn!(
                session_id = %request.session_id,
                request_id = %request.request_id,
                "ignoring a permission request from a session that is no longer running"
            );
            return;
        };

        let args = request.input.to_string();
        match bridge.request_permission_with_id(
            &request.request_id,
            &request.session_id,
            &request.tool_name,
            &args,
        ) {
            Ok(request_id) => {
                let (answered, answered_rx) = tokio::sync::oneshot::channel();
                self.pending.insert(
                    request_id.clone(),
                    PendingChildPermission {
                        session_id: request.session_id.clone(),
                        decisions,
                        _answered: answered,
                    },
                );

                if let Some(timeout) = self.timeout {
                    let timeout_tx = self.timeout_tx.clone();
                    let timed_out_id = request_id.clone();
                    tokio::spawn(async move {
                        // An answer drops the sender and completes this wait
                        // early; only an elapsed deadline means the request
                        // went unanswered.
                        if tokio::time::timeout(timeout, answered_rx).await.is_err() {
                            let _ = timeout_tx.send(timed_out_id);
                        }
                    });
                }

                let first = {
                    let count = self
                        .session_counts
                        .entry(request.session_id.clone())
                        .or_insert(0);
                    *count += 1;
                    *count == 1
                };
                if first && let Err(error) = bridge.begin_awaiting_permission(&request.session_id) {
                    tracing::warn!(
                        session_id = %request.session_id,
                        error = %error,
                        "could not record the session as awaiting permission"
                    );
                }

                // Route the answer before the frame is flushed: the person
                // can answer as soon as it appears in the gateway.
                if let Err(error) = bridge.send_pending_permission_requests().await {
                    tracing::warn!(error = %error, "could not send a permission request");
                }
            }
            Err(error) => tracing::warn!(
                session_id = %request.session_id,
                error = %error,
                "ignoring a child permission request with a known id"
            ),
        }
    }

    /// Deliver the router's answer to the child and leave `AwaitingPermission`
    /// when this was the session's last outstanding request.
    fn deliver(&mut self, bridge: &mut Bridge, request_id: &str) {
        let Some(entry) = self.pending.remove(request_id) else {
            // The timeout path already answered this request.
            return;
        };
        self.release_session(bridge, &entry.session_id);

        let (allow, allow_session, message) = match bridge.get_permission_response(request_id) {
            Some(PermissionResponse::Allow { always }) => (
                true,
                *always,
                if *always {
                    "approved for the session"
                } else {
                    "approved"
                },
            ),
            // A response the router no longer has (for example after a
            // restart) is treated as a denial: silence must not become
            // consent.
            Some(PermissionResponse::Deny) | None => (false, false, "denied"),
        };

        if entry
            .decisions
            .send(ChildPermissionDecision {
                request_id: request_id.to_string(),
                allow,
                allow_session: allow_session && allow,
                message: message.to_string(),
            })
            .is_err()
        {
            tracing::warn!("session ended before its permission decision could be delivered");
        }
    }

    /// Apply an elapsed bounded wait.
    fn expire(&mut self, bridge: &mut Bridge, request_id: &str) {
        if !self.pending.contains_key(request_id) {
            // Already answered; the timer raced the answer.
            return;
        }

        // The gateway may have resolved the router in a serve-loop turn that
        // has not reached the event branch yet. That answer wins.
        if bridge.is_permission_resolved(request_id) {
            self.deliver(bridge, request_id);
            return;
        }

        let Some(entry) = self.pending.remove(request_id) else {
            return;
        };
        self.release_session(bridge, &entry.session_id);

        let message = self.timeout.map_or_else(
            || "approval timed out; no decision was received".to_string(),
            |timeout| {
                format!(
                    "approval timed out after {timeout:?}; no decision was received, so the \
                     tool was not run"
                )
            },
        );
        tracing::warn!(
            session_id = %entry.session_id,
            request_id = %request_id,
            "permission approval timed out; denying the request"
        );

        // Resolve the bridge-side router as Deny. The first answer wins, so a
        // gateway response that arrives after this cannot revive the request.
        let _ = bridge.expire_permission(request_id);

        if entry
            .decisions
            .send(ChildPermissionDecision {
                request_id: request_id.to_string(),
                allow: false,
                allow_session: false,
                message,
            })
            .is_err()
        {
            tracing::warn!("session ended before its timeout denial could be delivered");
        }
    }

    /// Drop everything tracked for a finished session.
    fn forget_session(&mut self, session_id: &str) {
        self.pending.retain(|_, p| p.session_id != session_id);
        self.session_counts.remove(session_id);
    }

    /// Decrement the session's outstanding count and return it to `Running`
    /// when nothing is left pending.
    fn release_session(&mut self, bridge: &mut Bridge, session_id: &str) {
        let Some(count) = self.session_counts.get_mut(session_id) else {
            return;
        };
        if *count > 1 {
            *count -= 1;
            return;
        }
        self.session_counts.remove(session_id);
        if let Err(error) = bridge.end_awaiting_permission(session_id) {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "could not return the session to running after its permission"
            );
        }
    }
}

/// Errors raised while turning CLI arguments into a launch configuration.
#[derive(Debug, thiserror::Error)]
pub enum BridgeConfigError {
    /// One or more required values were not supplied by flag or environment.
    #[error("missing required bridge configuration: {0}")]
    MissingRequired(String),
    /// A supplied value cannot be used as given.
    #[error("invalid bridge configuration: {0}")]
    Invalid(String),
}

/// A fully resolved bridge launch configuration.
#[derive(Debug, Clone)]
pub struct BridgeLaunchConfig {
    /// Control-plane base URL (HTTP API).
    pub control_plane_url: String,
    /// Environment identifier.
    pub env_id: String,
    /// Git repository root.
    pub repo_root: PathBuf,
    /// Base directory for worktrees.
    pub worktree_base: PathBuf,
    /// Base directory for session data.
    pub session_base: PathBuf,
    /// Base directory for archives.
    pub archive_base: PathBuf,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Heartbeat settings.
    pub heartbeat: HeartbeatConfig,
    /// Registered transport kind.
    pub transport: String,
    /// Transport endpoint.
    pub transport_url: String,
    /// Connection identifier reported by the transport.
    pub connection_id: String,
    /// Optional bearer token.
    pub auth_token: Option<String>,
    /// How often to fetch assignments and report capacity.
    pub poll_interval: Duration,
    /// Path template used to claim an assignment before running it.
    pub claim_path: String,
    /// How long a child's permission request may wait for an answer.
    ///
    /// `None` means no bound: the request waits until it is answered or the
    /// child exits, which is the historical behaviour selected explicitly
    /// with `CODEREX_BRIDGE_APPROVAL_TIMEOUT_SECS=0`.
    pub approval_timeout: Option<Duration>,
}

impl BridgeLaunchConfig {
    /// Resolve flags, falling back to `env` for values the parser left empty.
    ///
    /// In production `env` is `std::env::var(..).ok()`; tests inject a map so
    /// the mapping can be checked without mutating the process environment.
    pub fn resolve<F>(args: &BridgeArgs, cwd: &Path, env: F) -> Result<Self, BridgeConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let control_plane_url = optional_string(
            args.control_plane_url.as_deref(),
            env(ENV_CONTROL_PLANE_URL),
        );
        let env_id = optional_string(args.env_id.as_deref(), env(ENV_ENV_ID));

        // Both required values are reported together; a person running the
        // command for the first time should not have to fix one at a time.
        if control_plane_url.is_none() || env_id.is_none() {
            let mut missing = Vec::new();
            if control_plane_url.is_none() {
                missing.push(format!("--control-plane-url (${ENV_CONTROL_PLANE_URL})"));
            }
            if env_id.is_none() {
                missing.push(format!("--env-id (${ENV_ENV_ID})"));
            }
            return Err(BridgeConfigError::MissingRequired(missing.join(", ")));
        }
        // The poller appends `/api/...`, so a trailing slash would produce a
        // doubled separator.
        let control_plane_url = control_plane_url
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        let env_id = env_id.unwrap_or_default();

        let repo_root = optional_path(args.repo_root.clone(), env(ENV_REPO_ROOT))
            .unwrap_or_else(|| cwd.to_path_buf());
        if !repo_root.is_dir() {
            return Err(BridgeConfigError::Invalid(format!(
                "repository root {} is not a directory",
                repo_root.display()
            )));
        }

        let worktree_base = optional_path(args.worktree_base.clone(), env(ENV_WORKTREE_BASE))
            .unwrap_or_else(|| repo_root.join(".coderex/worktrees"));
        let session_base = optional_path(args.session_base.clone(), env(ENV_SESSION_BASE))
            .unwrap_or_else(|| repo_root.join(".coderex/sessions"));
        let archive_base = optional_path(args.archive_base.clone(), env(ENV_ARCHIVE_BASE))
            .unwrap_or_else(|| repo_root.join(".coderex/archives"));

        if args.max_concurrent == 0 {
            return Err(BridgeConfigError::Invalid(
                "--max-concurrent must be at least 1".into(),
            ));
        }
        if args.heartbeat_interval_secs == 0 {
            return Err(BridgeConfigError::Invalid(
                "--heartbeat-interval-secs must be at least 1".into(),
            ));
        }
        if args.heartbeat_timeout_secs == 0 {
            return Err(BridgeConfigError::Invalid(
                "--heartbeat-timeout-secs must be at least 1".into(),
            ));
        }
        if args.heartbeat_degraded_threshold == 0 {
            return Err(BridgeConfigError::Invalid(
                "--heartbeat-degraded-threshold must be at least 1".into(),
            ));
        }
        if args.heartbeat_lost_threshold < args.heartbeat_degraded_threshold {
            return Err(BridgeConfigError::Invalid(format!(
                "--heartbeat-lost-threshold ({}) must be at least the degraded threshold ({})",
                args.heartbeat_lost_threshold, args.heartbeat_degraded_threshold
            )));
        }
        if args.poll_interval_secs == 0 {
            return Err(BridgeConfigError::Invalid(
                "--poll-interval-secs must be at least 1".into(),
            ));
        }

        let transport = optional_string(Some(args.transport.as_str()), env(ENV_TRANSPORT))
            .unwrap_or_else(|| DEFAULT_TRANSPORT.to_string());
        let transport_url = optional_string(args.transport_url.as_deref(), env(ENV_TRANSPORT_URL))
            .unwrap_or_else(|| derive_transport_url(&control_plane_url));
        let connection_id = optional_string(args.connection_id.as_deref(), env(ENV_CONNECTION_ID))
            .unwrap_or_else(|| env_id.clone());
        let auth_token = optional_string(args.auth_token.as_deref(), env(ENV_AUTH_TOKEN));
        let claim_path = optional_string(args.claim_path.as_deref(), env(ENV_CLAIM_PATH))
            .unwrap_or_else(|| DEFAULT_CLAIM_PATH_TEMPLATE.to_string());
        if !claim_path.contains("{assignment_id}") {
            return Err(BridgeConfigError::Invalid(format!(
                "--claim-path must contain {{assignment_id}}, got '{claim_path}'"
            )));
        }

        // A bound of zero is the explicit opt-out; any other value has to be
        // a whole number of seconds.
        let approval_timeout = match env(ENV_APPROVAL_TIMEOUT_SECS) {
            None => Some(Duration::from_secs(DEFAULT_APPROVAL_TIMEOUT_SECS)),
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(0) => None,
                Ok(secs) => Some(Duration::from_secs(secs)),
                Err(_) => {
                    return Err(BridgeConfigError::Invalid(format!(
                        "${ENV_APPROVAL_TIMEOUT_SECS} must be a whole number of seconds \
                         (0 disables the bound), got '{raw}'"
                    )));
                }
            },
        };

        if is_websocket(&transport) && !is_websocket_url(&transport_url) {
            return Err(BridgeConfigError::Invalid(format!(
                "transport '{transport}' needs a ws:// or wss:// URL, got '{transport_url}' \
                 (set --transport-url or ${ENV_TRANSPORT_URL})"
            )));
        }

        Ok(Self {
            control_plane_url,
            env_id,
            repo_root,
            worktree_base,
            session_base,
            archive_base,
            max_concurrent: args.max_concurrent,
            heartbeat: HeartbeatConfig {
                interval_secs: args.heartbeat_interval_secs,
                timeout_secs: args.heartbeat_timeout_secs,
                degraded_threshold: args.heartbeat_degraded_threshold,
                lost_threshold: args.heartbeat_lost_threshold,
            },
            transport,
            transport_url,
            connection_id,
            auth_token,
            poll_interval: Duration::from_secs(args.poll_interval_secs),
            claim_path,
            approval_timeout,
        })
    }

    /// Build the orchestrator configuration.
    #[must_use]
    pub fn bridge_config(&self) -> BridgeConfig {
        BridgeConfig {
            env_id: self.env_id.clone(),
            max_concurrent: self.max_concurrent,
            control_plane_url: self.control_plane_url.clone(),
            repo_root: self.repo_root.clone(),
            worktree_base: self.worktree_base.clone(),
            session_base: self.session_base.clone(),
            archive_base: self.archive_base.clone(),
            heartbeat: self.heartbeat,
        }
    }

    /// Endpoint label for diagnostics, with credentials left out.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        if is_websocket(&self.transport) {
            &self.transport_url
        } else {
            &self.env_id
        }
    }
}

/// Build the production transport from the resolved configuration.
///
/// Returns an error for an unregistered transport kind; it never silently
/// substitutes a no-op transport.
pub fn create_transport(config: &BridgeLaunchConfig) -> Result<Box<dyn Transport>> {
    let factory = default_factory();
    // The HTTP-poll transport addresses the gateway's per-environment frame
    // mailbox, so it needs the environment id. `connection_id` is a free-form
    // label that `--connection-id` may override; using it here would silently
    // read another environment's mailbox.
    let connection_id = if is_polling_transport(&config.transport) {
        config.env_id.clone()
    } else {
        config.connection_id.clone()
    };
    let transport_config = TransportConfig {
        kind: config.transport.clone(),
        endpoint: config.endpoint().to_string(),
        connection_id,
        auth_token: config.auth_token.clone(),
    };

    factory.create_configured(&transport_config).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown bridge transport '{}'; available transports: {}",
            config.transport,
            factory.registered_types().join(", ")
        )
    })
}

/// Whether a transport kind reads the gateway's frame mailbox.
///
/// Those address an environment, not a connection label.
#[must_use]
fn is_polling_transport(kind: &str) -> bool {
    matches!(kind, "http-poll" | "http")
}

/// Resolve the configuration, build the transport, and serve until shutdown.
pub async fn run(args: BridgeArgs, cwd: &Path) -> Result<()> {
    let config = BridgeLaunchConfig::resolve(&args, cwd, |key| std::env::var(key).ok())
        .context("cannot start the bridge")?;
    let transport = create_transport(&config)?;

    tracing::info!(
        env_id = %config.env_id,
        transport = %config.transport,
        endpoint = %config.endpoint(),
        control_plane = %config.control_plane_url,
        max_concurrent = config.max_concurrent,
        "starting bridge worker"
    );

    run_with_transport(config, transport, shutdown_signal()).await
}

/// Run a bridge over an injected transport until `shutdown` resolves.
///
/// This is the seam used by tests: the transport is supplied by the caller
/// and no process-wide signal handler is installed. The control plane is the
/// HTTP client built from `config.control_plane_url`.
pub async fn run_with_transport<F>(
    config: BridgeLaunchConfig,
    transport: Box<dyn Transport>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut control_plane = HttpControlPlaneClient::new(config.control_plane_url.clone())
        .with_claim_path(config.claim_path.clone());
    if let Some(ref token) = config.auth_token {
        control_plane = control_plane.with_auth(token.clone());
    }
    run_with_control_plane(
        config,
        transport,
        Arc::new(control_plane),
        Arc::new(ProcessSessionRunner::default()),
        shutdown,
    )
    .await
}

/// Run a bridge over injected transport, control-plane, and session runners.
///
/// The dependencies are supplied by the caller, so tests can exercise
/// registration, assignment spawning, session execution, capacity reporting,
/// and shutdown without a live server or a child process.
pub async fn run_with_control_plane<F>(
    config: BridgeLaunchConfig,
    transport: Box<dyn Transport>,
    control_plane: Arc<dyn ControlPlaneClient>,
    runner: Arc<dyn SessionRunner>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut bridge = Bridge::with_control_plane(config.bridge_config(), transport, control_plane);

    bridge.connect().await.map_err(|err| {
        anyhow::anyhow!(
            "bridge transport '{}' could not connect to {}: {err}",
            config.transport,
            config.endpoint()
        )
    })?;

    let stop = bridge.stop_handle();
    let shutdown_stop = stop.clone();
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        shutdown_stop.stop();
    });

    let result = run_worker(&mut bridge, &config, &stop, runner).await;

    let _ = bridge.disconnect().await;
    shutdown_task.abort();
    result
}

/// Register, then run the assignment and transport loops until shutdown.
async fn run_worker(
    bridge: &mut Bridge,
    config: &BridgeLaunchConfig,
    stop: &BridgeStopHandle,
    runner: Arc<dyn SessionRunner>,
) -> Result<()> {
    register_with_retry(bridge, stop)
        .await
        .context("cannot start the bridge worker")?;

    let initial_report = tokio::select! {
        result = bridge.report_capacity_to_server() => result,
        () = stop.wait() => return Ok(()),
    };
    if let Err(err) = initial_report {
        tracing::warn!("initial capacity report failed: {err}");
    }

    let mut ticker = tokio::time::interval(config.poll_interval);
    // The first tick completes immediately, so the first assignment fetch and
    // capacity report happen as soon as registration succeeds.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let (completion_tx, mut completion_rx) =
        tokio::sync::mpsc::unbounded_channel::<SessionCompletion>();
    // Lines a session's readers produce. The original sender stays alive for
    // the life of the worker so `recv` never reports "all senders dropped"
    // while sessions can still be spawned.
    let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel::<SessionOutputLine>();
    // Permission requests the children emit. The launcher owns the bridge
    // (and its transport), so the request has to travel back here before it
    // can become a `permission_request` frame.
    let (permission_request_tx, mut permission_request_rx) =
        tokio::sync::mpsc::unbounded_channel::<ChildPermissionRequest>();
    // Expired bounded waits report their request id here; the tracker decides
    // whether the answer or the deadline won.
    let (permission_timeout_tx, mut permission_timeout_rx) =
        tokio::sync::mpsc::unbounded_channel::<String>();
    let mut permissions = PermissionTracker::new(config.approval_timeout, permission_timeout_tx);
    // Subscribed once: `respond_to_permission` broadcasts the resolution, and
    // the serve loop may complete at any point in the select.
    let mut bridge_events = bridge.subscribe();
    let mut running: HashMap<String, RunningSession> = HashMap::new();
    let mut serve_error = None;

    loop {
        if stop.is_stopped() {
            break;
        }

        tokio::select! {
            served = bridge.serve_transport() => {
                // Exhausted reconnect attempts surface as an error here, and
                // the worker exits rather than silently sitting on a dead
                // transport.
                if let Err(err) = served {
                    serve_error = Some(err);
                } else {
                    tracing::info!("bridge transport serve loop ended");
                }
                break;
            }
            done = completion_rx.recv() => {
                if let Some(done) = done {
                    running.remove(&done.session_id);
                    permissions.forget_session(&done.session_id);
                    finish_session(bridge, done).await;
                    // `finish_session` queued the terminal state; the serve
                    // loop owns the transport, so flush while we have it.
                    flush_session_writes(bridge).await;
                }
            }
            line = output_rx.recv() => {
                if let Some(line) = line
                    && let Err(err) = bridge
                        .forward_session_output(&line.session_id, line.stream, &line.line)
                        .await
                    {
                        tracing::warn!(
                            session_id = %line.session_id,
                            error = %err,
                            "could not forward session output"
                        );
                    }
            }
            request = permission_request_rx.recv() => {
                if let Some(request) = request {
                    permissions.register(bridge, &running, request).await;
                }
            }
            request_id = permission_timeout_rx.recv() => {
                if let Some(request_id) = request_id {
                    permissions.expire(bridge, &request_id);
                }
            }
            event = bridge_events.recv() => {
                // Every event other than a resolution is informational here;
                // lagging is fine because the resolution is also visible in
                // the router (`get_permission_response`).
                if let Ok(BridgeEvent::PermissionResolved { request_id, .. }) = event {
                    permissions.deliver(bridge, &request_id);
                }
            }
            _ = ticker.tick() => {
                if stop.is_stopped() {
                    break;
                }
                let spawned = assignment_tick(bridge, stop, &config.session_base).await;
                if stop.is_stopped() {
                    for session in spawned {
                        bridge.fail_session(
                            &session.session_id,
                            "bridge shut down before the session started".into(),
                        );
                    }
                } else {
                    spawn_session_tasks(
                        spawned,
                        &runner,
                        &completion_tx,
                        &output_tx,
                        &permission_request_tx,
                        &bridge.process_registry(),
                        &mut running,
                    );
                }
                // Permission requests created by a bridge-side producer wait
                // here until this loop owns the transport again.
                if let Err(err) = bridge.send_pending_permission_requests().await {
                    tracing::warn!(error = %err, "could not send pending permission requests");
                }
                // State writes queued by a successful spawn wait for the
                // same moment: the loop owns the transport here.
                flush_session_writes(bridge).await;
            }
        }
    }

    stop_running_sessions(bridge, &mut running).await;
    tracing::info!("bridge worker stopped");

    serve_error.map_or_else(
        || Ok(()),
        |err| Err(anyhow::anyhow!("bridge serve loop failed: {err}")),
    )
}

/// Start one task per spawned session and remember it for shutdown.
fn spawn_session_tasks(
    sessions: Vec<SpawnedSession>,
    runner: &Arc<dyn SessionRunner>,
    completion_tx: &tokio::sync::mpsc::UnboundedSender<SessionCompletion>,
    output_tx: &tokio::sync::mpsc::UnboundedSender<SessionOutputLine>,
    permission_request_tx: &tokio::sync::mpsc::UnboundedSender<ChildPermissionRequest>,
    process_registry: &ProcessRegistry,
    running: &mut HashMap<String, RunningSession>,
) {
    for session in sessions {
        let session_id = session.session_id.clone();
        let completion_tx = completion_tx.clone();
        let output_tx = output_tx.clone();
        let permission_request_tx = permission_request_tx.clone();
        let runner = Arc::clone(runner);
        // The bridge's own registry, so the runner's child is visible to the
        // cleanup that runs when the session ends.
        let process_registry = process_registry.clone();
        let stop = SessionStop::new();
        let task_stop = stop.clone();
        // One decision channel per child. The task owns the receiving end
        // (through the slot) and the running-session entry keeps the sending
        // end alive; per-request clones go into the launcher's pending map.
        let (permission_tx, permission_rx) =
            tokio::sync::mpsc::unbounded_channel::<ChildPermissionDecision>();
        let decision_slot: PermissionDecisionSlot =
            Arc::new(std::sync::Mutex::new(Some(permission_rx)));
        let handle = tokio::spawn(async move {
            let context = SessionContext {
                session_id: &session.session_id,
                prompt: &session.prompt,
                work_dir: &session.work_dir,
                session_dir: &session.session_dir,
                output: Some(output_tx),
                permission_requests: Some(permission_request_tx),
                permission_decisions: Some(decision_slot),
                process_registry: Some(process_registry),
            };
            let outcome = runner.run(&context, task_stop).await;
            let _ = completion_tx.send(SessionCompletion {
                session_id: session.session_id,
                outcome,
            });
        });
        running.insert(
            session_id,
            RunningSession {
                handle,
                stop,
                permission_tx,
            },
        );
    }
}

/// Queue a fenced state write for the control plane.
///
/// A worker registered without an epoch (the control plane did not report
/// one) cannot produce a write the gateway can order; that is a deployment
/// limitation, not a reason to stop the session, so it is warned about once
/// and otherwise ignored.
fn queue_session_state(bridge: &mut Bridge, session_id: &str, state: &str) {
    if let Err(error) = bridge.queue_session_write(session_id, state) {
        static WARNED_NO_EPOCH: AtomicBool = AtomicBool::new(false);
        if !WARNED_NO_EPOCH.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                error = %error,
                "not sending session state writes: registration assigned no worker epoch"
            );
        }
    }
}

/// Flush queued state writes over the transport.
async fn flush_session_writes(bridge: &mut Bridge) {
    if let Err(err) = bridge.send_pending_session_writes().await {
        tracing::warn!(error = %err, "could not send pending session writes");
    }
}

/// Apply a finished session's outcome and free its capacity slot.
async fn finish_session(bridge: &mut Bridge, done: SessionCompletion) {
    let final_state = match done.outcome {
        Ok(output) => {
            if let Err(err) = bridge
                .complete_session(&done.session_id, &output.transcript, output.usage)
                .await
            {
                tracing::warn!(
                    session_id = %done.session_id,
                    error = %err,
                    "failed to complete bridge session"
                );
                // `complete_session` frees the slot only after it succeeds.
                bridge.fail_session(&done.session_id, err);
                "failed"
            } else {
                tracing::info!(session_id = %done.session_id, "bridge session completed");
                "completed"
            }
        }
        Err(err) => {
            tracing::warn!(
                session_id = %done.session_id,
                error = %err,
                "bridge session failed"
            );
            bridge.fail_session(&done.session_id, err);
            "failed"
        }
    };
    queue_session_state(bridge, &done.session_id, final_state);
}

/// Ask running sessions to stop, then release their slots.
///
/// Each runner gets a cancellation signal and a bounded grace period: the
/// default runner sends `SIGTERM` (or terminates the child on Windows) and
/// waits before killing it. A runner that ignores the signal is aborted when
/// the grace period expires, and `kill_on_drop` prevents an orphan process.
async fn stop_running_sessions(bridge: &mut Bridge, running: &mut HashMap<String, RunningSession>) {
    for session in running.values() {
        session.stop.cancel();
    }

    let deadline = tokio::time::Instant::now() + SESSION_TASK_SHUTDOWN_GRACE;
    for (session_id, mut session) in running.drain() {
        if tokio::time::timeout_at(deadline, &mut session.handle)
            .await
            .is_err()
        {
            tracing::warn!(
                session_id = %session_id,
                "session did not stop within the grace period; aborting its task"
            );
            session.handle.abort();
            let _ = session.handle.await;
        }
        bridge.fail_session(
            &session_id,
            "bridge shut down while the session was running".into(),
        );
    }
}

/// Fetch work, spawn whatever fits, and report capacity.
///
/// The fetch and capacity requests are cancel-safe against shutdown: a stop
/// request does not wait for an in-flight HTTP call. Spawning is not
/// interrupted, because a half-spawned session would leak a capacity slot.
async fn assignment_tick(
    bridge: &mut Bridge,
    stop: &BridgeStopHandle,
    session_base: &Path,
) -> Vec<SpawnedSession> {
    let mut spawned = Vec::new();

    let fetched = tokio::select! {
        result = bridge.fetch_assignments() => result,
        () = stop.wait() => return spawned,
    };

    match fetched {
        Ok(0) => {}
        Ok(count) => tracing::info!(count, "fetched bridge assignments"),
        Err(err) => tracing::warn!("failed to fetch bridge assignments: {err}"),
    }

    while let Some(assignment) = bridge.take_assignment() {
        // Claim after taking the slot and before creating any resources. A
        // claim is a compare-and-set: another worker may have won the race,
        // in which case there is no session to run and the slot is released.
        match bridge.claim_assignment(&assignment).await {
            Ok(ClaimOutcome::Claimed) => {}
            Ok(ClaimOutcome::Lost) => {
                tracing::info!(
                    assignment_id = %assignment.assignment_id,
                    session_id = %assignment.session_id,
                    "assignment was already claimed or no longer exists; skipping"
                );
                bridge.release_assignment_slot();
                continue;
            }
            Err(err) => {
                tracing::warn!(
                    assignment_id = %assignment.assignment_id,
                    session_id = %assignment.session_id,
                    error = %err,
                    "failed to claim bridge assignment"
                );
                bridge.fail_session(
                    &assignment.session_id,
                    format!("assignment claim failed: {err}"),
                );
                continue;
            }
        }

        match bridge.spawn_session(&assignment).await {
            Ok(result) => {
                tracing::info!(
                    session_id = %result.session_id,
                    work_dir = %result.work_dir.display(),
                    "spawned bridge session"
                );
                // The runner is about to start this session's child, so the
                // spawn edge is complete. Drive the validated state machine
                // from Spawning to Running before any permission request can
                // arrive from the child.
                if let Err(error) = bridge.mark_session_running(&result.session_id) {
                    tracing::warn!(
                        session_id = %result.session_id,
                        error = %error,
                        "could not record the session as running"
                    );
                }
                // Announce the transition the same way `finish_session`
                // announces completion; the launcher flushes it when the
                // tick returns.
                queue_session_state(bridge, &result.session_id, "running");
                spawned.push(SpawnedSession {
                    session_dir: session_base.join(&result.session_id),
                    session_id: result.session_id,
                    work_dir: result.work_dir,
                    prompt: assignment.prompt,
                });
            }
            Err(err) => {
                tracing::warn!(
                    session_id = %assignment.session_id,
                    error = %err,
                    "failed to spawn bridge session"
                );
                // Frees the capacity slot so the next assignment can be taken.
                bridge.fail_session(&assignment.session_id, err);
            }
        }
    }

    if stop.is_stopped() {
        return spawned;
    }

    let reported = tokio::select! {
        result = bridge.report_capacity_to_server() => result,
        () = stop.wait() => return spawned,
    };
    if let Err(err) = reported {
        tracing::warn!("failed to report bridge capacity: {err}");
    }

    spawned
}

/// Register with bounded exponential backoff, giving up at shutdown.
async fn register_with_retry(bridge: &Bridge, stop: &BridgeStopHandle) -> Result<()> {
    let mut delay = Duration::from_millis(500);
    let mut last_error = String::new();

    for attempt in 1..=REGISTRATION_ATTEMPTS {
        let registered = tokio::select! {
            result = bridge.register() => result,
            () = stop.wait() => {
                return Err(anyhow::anyhow!(
                    "control-plane registration cancelled by shutdown"
                ));
            }
        };

        match registered {
            Ok(()) => {
                tracing::info!(
                    attempt,
                    worker_epoch = bridge.worker_epoch(),
                    "bridge registered with the control plane"
                );
                return Ok(());
            }
            Err(err) => last_error = err,
        }

        if attempt < REGISTRATION_ATTEMPTS {
            tracing::warn!(
                attempt,
                error = %last_error,
                retry_in_ms = delay.as_millis(),
                "bridge registration failed; retrying"
            );
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = stop.wait() => {
                    return Err(anyhow::anyhow!(
                        "control-plane registration cancelled by shutdown"
                    ));
                }
            }
            delay = (delay * 2).min(Duration::from_secs(10));
        }
    }

    Err(anyhow::anyhow!(
        "control-plane registration failed after {REGISTRATION_ATTEMPTS} attempts: {last_error}"
    ))
}

/// Wait for Ctrl+C, or SIGTERM on Unix, to request a graceful bridge stop.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
                tracing::warn!("cannot listen for SIGTERM ({err}); waiting for Ctrl+C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                match result {
                    Ok(()) => tracing::info!("Ctrl+C received — stopping bridge"),
                    Err(err) => tracing::warn!("Ctrl+C listener failed: {err}"),
                }
            }
            _ = terminate.recv() => tracing::info!("SIGTERM received — stopping bridge"),
        }
    }

    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => tracing::info!("Ctrl+C received — stopping bridge"),
            Err(err) => tracing::warn!("Ctrl+C listener failed: {err}"),
        }
    }
}

/// Rewrite an HTTP control-plane URL to its WebSocket scheme.
///
/// The path is kept as-is; deployments whose gateway lives on a different
/// path should pass `--transport-url` explicitly.
#[must_use]
pub fn derive_transport_url(control_plane_url: &str) -> String {
    let trimmed = control_plane_url.trim_end_matches('/');
    match (
        trimmed.strip_prefix("https://"),
        trimmed.strip_prefix("http://"),
    ) {
        (Some(rest), _) => format!("wss://{rest}"),
        (None, Some(rest)) => format!("ws://{rest}"),
        (None, None) => trimmed.to_string(),
    }
}

fn optional_string(arg: Option<&str>, env: Option<String>) -> Option<String> {
    arg.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            env.map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn optional_path(arg: Option<PathBuf>, env: Option<String>) -> Option<PathBuf> {
    arg.or_else(|| optional_string(None, env).map(PathBuf::from))
}

const fn is_websocket(transport: &str) -> bool {
    transport.eq_ignore_ascii_case("websocket") || transport.eq_ignore_ascii_case("ws")
}

fn is_websocket_url(url: &str) -> bool {
    url.starts_with("ws://") || url.starts_with("wss://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use coderex_bridge::poller::{CapacityReport, SpawnMode, WorkAssignment};
    use coderex_bridge::state::BridgeSessionState;
    use coderex_bridge::transport::{InProcessTransport, TransportMessage};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::oneshot;

    /// In-memory control plane used to exercise the worker without a server.
    #[derive(Debug, Default)]
    struct FakeControlPlane {
        registrations: Mutex<Vec<(String, usize)>>,
        assignments: Mutex<Vec<WorkAssignment>>,
        capacities: Mutex<Vec<CapacityReport>>,
        fetches: Mutex<usize>,
        claims: Mutex<Vec<(String, String)>>,
        /// Epoch returned by registration; `0` means "not reported".
        worker_epoch: u64,
        /// Outcomes returned by successive claim calls; empty means `Claimed`.
        claim_outcomes: Mutex<std::collections::VecDeque<Result<ClaimOutcome, String>>>,
    }

    impl FakeControlPlane {
        fn queue_claim_outcomes(&self, outcomes: Vec<Result<ClaimOutcome, String>>) {
            self.claim_outcomes.lock().unwrap().extend(outcomes);
        }

        fn claimed(&self) -> Vec<(String, String)> {
            self.claims.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ControlPlaneClient for FakeControlPlane {
        async fn register(&self, env_id: &str, max_concurrent: usize) -> Result<u64, String> {
            self.registrations
                .lock()
                .unwrap()
                .push((env_id.to_string(), max_concurrent));
            Ok(self.worker_epoch)
        }

        async fn fetch_assignments(&self, _env_id: &str) -> Result<Vec<WorkAssignment>, String> {
            *self.fetches.lock().unwrap() += 1;
            Ok(std::mem::take(&mut *self.assignments.lock().unwrap()))
        }

        async fn claim_assignment(
            &self,
            env_id: &str,
            assignment_id: &str,
        ) -> Result<ClaimOutcome, String> {
            self.claims
                .lock()
                .unwrap()
                .push((env_id.to_string(), assignment_id.to_string()));
            self.claim_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(ClaimOutcome::Claimed))
        }

        async fn report_capacity(
            &self,
            _env_id: &str,
            report: &CapacityReport,
        ) -> Result<(), String> {
            self.capacities.lock().unwrap().push(report.clone());
            Ok(())
        }
    }

    fn sample_assignment(session_base: &Path) -> WorkAssignment {
        WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: session_base.display().to_string(),
            prompt: "Fix the bug".into(),
            spawn_mode: SpawnMode::SingleSession,
        }
    }

    /// Runner that finishes immediately with a fixed outcome.
    #[derive(Debug)]
    struct FixedRunner {
        outcome: Result<(), String>,
        runs: Mutex<Vec<String>>,
    }

    impl FixedRunner {
        fn succeeding() -> Self {
            Self {
                outcome: Ok(()),
                runs: Mutex::new(Vec::new()),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                outcome: Err(message.to_string()),
                runs: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionRunner for FixedRunner {
        async fn run(
            &self,
            context: &SessionContext<'_>,
            _stop: SessionStop,
        ) -> Result<SessionOutput, String> {
            self.runs
                .lock()
                .unwrap()
                .push(context.session_id.to_string());
            self.outcome.clone().map(|()| SessionOutput {
                transcript: "{\"type\":\"result\",\"subtype\":\"success\"}\n".into(),
                usage: UsageStats::default(),
            })
        }
    }

    /// Runner that never finishes on its own; shutdown must cancel it.
    #[derive(Debug, Default)]
    struct PendingRunner {
        started: Arc<tokio::sync::Notify>,
        cancelled: Arc<AtomicBool>,
    }

    struct CancelFlag(Arc<AtomicBool>);

    impl Drop for CancelFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl SessionRunner for PendingRunner {
        async fn run(
            &self,
            _context: &SessionContext<'_>,
            stop: SessionStop,
        ) -> Result<SessionOutput, String> {
            let _cancel_flag = CancelFlag(Arc::clone(&self.cancelled));
            self.started.notify_one();
            stop.wait().await;
            Err("session cancelled by shutdown".into())
        }
    }

    /// Runner that emits output lines the way the process runner's readers
    /// do, then finishes successfully.
    #[derive(Debug)]
    struct StreamingRunner {
        lines: Vec<(&'static str, &'static str)>,
    }

    #[async_trait::async_trait]
    impl SessionRunner for StreamingRunner {
        async fn run(
            &self,
            context: &SessionContext<'_>,
            _stop: SessionStop,
        ) -> Result<SessionOutput, String> {
            if let Some(output) = &context.output {
                for (stream, line) in &self.lines {
                    let _ = output.send(SessionOutputLine {
                        session_id: context.session_id.to_string(),
                        stream,
                        line: (*line).to_string(),
                    });
                }
            }
            Ok(SessionOutput {
                transcript: "{\"type\":\"result\",\"subtype\":\"success\"}\n".into(),
                usage: UsageStats::default(),
            })
        }
    }

    /// Build a resolved config with fast polling for worker tests.
    fn worker_config(tmp: &Path) -> BridgeLaunchConfig {
        let mut args = args_with("http://cp.example.test", "env-test");
        args.transport = "in-process".into();
        args.repo_root = Some(tmp.to_path_buf());
        args.session_base = Some(tmp.join("sessions"));
        args.archive_base = Some(tmp.join("archives"));
        let mut config = BridgeLaunchConfig::resolve(&args, tmp, |_| None).unwrap();
        config.poll_interval = Duration::from_millis(10);
        config
    }

    /// Wait until `check` returns true, or fail the test after two seconds.
    async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    fn args_with(url: &str, env_id: &str) -> BridgeArgs {
        BridgeArgs {
            control_plane_url: Some(url.into()),
            env_id: Some(env_id.into()),
            repo_root: None,
            worktree_base: None,
            session_base: None,
            archive_base: None,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            transport: DEFAULT_TRANSPORT.into(),
            transport_url: None,
            connection_id: None,
            auth_token: None,
            heartbeat_interval_secs: 30,
            heartbeat_timeout_secs: 10,
            heartbeat_degraded_threshold: 2,
            heartbeat_lost_threshold: 5,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            claim_path: None,
        }
    }

    #[test]
    fn maps_every_field_from_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let args = BridgeArgs {
            control_plane_url: Some("https://cp.example.test".into()),
            env_id: Some("env-1".into()),
            repo_root: Some(repo.clone()),
            worktree_base: Some(tmp.path().join("wt")),
            session_base: Some(tmp.path().join("sessions")),
            archive_base: Some(tmp.path().join("archives")),
            max_concurrent: 7,
            transport: "websocket".into(),
            transport_url: Some("wss://gateway.example.test/bridge".into()),
            connection_id: Some("conn-1".into()),
            auth_token: Some("secret".into()),
            heartbeat_interval_secs: 15,
            heartbeat_timeout_secs: 4,
            heartbeat_degraded_threshold: 3,
            heartbeat_lost_threshold: 9,
            poll_interval_secs: 9,
            claim_path: Some("/api/assignments/{assignment_id}/claim".into()),
        };

        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();
        assert_eq!(config.control_plane_url, "https://cp.example.test");
        assert_eq!(config.env_id, "env-1");
        assert_eq!(config.repo_root, repo);
        assert_eq!(config.worktree_base, tmp.path().join("wt"));
        assert_eq!(config.session_base, tmp.path().join("sessions"));
        assert_eq!(config.archive_base, tmp.path().join("archives"));
        assert_eq!(config.max_concurrent, 7);
        assert_eq!(config.heartbeat.interval_secs, 15);
        assert_eq!(config.heartbeat.timeout_secs, 4);
        assert_eq!(config.heartbeat.degraded_threshold, 3);
        assert_eq!(config.heartbeat.lost_threshold, 9);
        assert_eq!(config.transport, "websocket");
        assert_eq!(config.transport_url, "wss://gateway.example.test/bridge");
        assert_eq!(config.connection_id, "conn-1");
        assert_eq!(config.auth_token.as_deref(), Some("secret"));
        assert_eq!(config.poll_interval, Duration::from_secs(9));
        assert_eq!(config.claim_path, "/api/assignments/{assignment_id}/claim");
    }

    #[test]
    fn parse_defaults_apply() {
        let parsed = Cli::try_parse_from([
            "coderex",
            "bridge",
            "--control-plane-url",
            "http://127.0.0.1:3000/",
            "--env-id",
            "env-local",
        ])
        .unwrap();

        let crate::cli::Command::Bridge(args) = parsed.command.unwrap() else {
            panic!("expected the bridge subcommand");
        };
        assert_eq!(args.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(args.transport, DEFAULT_TRANSPORT);
        assert_eq!(args.heartbeat_interval_secs, 30);
        assert_eq!(args.heartbeat_timeout_secs, 10);
        assert_eq!(args.heartbeat_degraded_threshold, 2);
        assert_eq!(args.heartbeat_lost_threshold, 5);
        assert_eq!(args.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);

        let cwd = std::env::current_dir().unwrap();
        let config = BridgeLaunchConfig::resolve(&args, &cwd, |_| None).unwrap();
        assert_eq!(config.repo_root, cwd);
        assert_eq!(config.worktree_base, cwd.join(".coderex/worktrees"));
        assert_eq!(config.session_base, cwd.join(".coderex/sessions"));
        assert_eq!(config.archive_base, cwd.join(".coderex/archives"));
        // The trailing slash is trimmed and the scheme is rewritten.
        assert_eq!(config.control_plane_url, "http://127.0.0.1:3000");
        assert_eq!(config.transport_url, "ws://127.0.0.1:3000");
        assert_eq!(config.connection_id, "env-local");
        assert!(config.auth_token.is_none());
        assert_eq!(config.claim_path, DEFAULT_CLAIM_PATH_TEMPLATE);
    }

    #[test]
    fn environment_fills_required_and_optional_values() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = args_with("ignored", "ignored");
        args.control_plane_url = None;
        args.env_id = None;
        args.repo_root = None;
        args.transport_url = None;

        let repo = tmp.path().join("env-repo");
        std::fs::create_dir(&repo).unwrap();
        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |key| match key {
            ENV_CONTROL_PLANE_URL => Some("http://cp.example.test".into()),
            ENV_ENV_ID => Some("env-from-env".into()),
            ENV_REPO_ROOT => Some(repo.display().to_string()),
            ENV_TRANSPORT_URL => Some("ws://gateway.example.test:9000".into()),
            ENV_AUTH_TOKEN => Some("token-from-env".into()),
            _ => None,
        })
        .unwrap();

        assert_eq!(config.control_plane_url, "http://cp.example.test");
        assert_eq!(config.env_id, "env-from-env");
        assert_eq!(config.repo_root, repo);
        assert_eq!(config.transport_url, "ws://gateway.example.test:9000");
        assert_eq!(config.auth_token.as_deref(), Some("token-from-env"));
    }

    #[test]
    fn approval_timeout_defaults_to_five_minutes_and_is_env_overridable() {
        let tmp = tempfile::tempdir().unwrap();
        let args = args_with("http://cp.example.test", "env-test");

        let default = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();
        assert_eq!(
            default.approval_timeout,
            Some(Duration::from_secs(DEFAULT_APPROVAL_TIMEOUT_SECS))
        );

        let configured = BridgeLaunchConfig::resolve(&args, tmp.path(), |key| {
            (key == ENV_APPROVAL_TIMEOUT_SECS).then(|| "45".to_string())
        })
        .unwrap();
        assert_eq!(configured.approval_timeout, Some(Duration::from_secs(45)));

        // Zero is the explicit request for the old, unbounded behaviour.
        let disabled = BridgeLaunchConfig::resolve(&args, tmp.path(), |key| {
            (key == ENV_APPROVAL_TIMEOUT_SECS).then(|| "0".to_string())
        })
        .unwrap();
        assert_eq!(disabled.approval_timeout, None);

        let invalid = BridgeLaunchConfig::resolve(&args, tmp.path(), |key| {
            (key == ENV_APPROVAL_TIMEOUT_SECS).then(|| "soon".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(invalid.contains(ENV_APPROVAL_TIMEOUT_SECS), "{invalid}");
    }

    #[test]
    fn missing_required_values_are_reported_together() {
        let mut args = args_with("ignored", "ignored");
        args.control_plane_url = None;
        args.env_id = None;

        let error = BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--control-plane-url"), "{message}");
        assert!(message.contains(ENV_CONTROL_PLANE_URL), "{message}");
        assert!(message.contains("--env-id"), "{message}");
        assert!(message.contains(ENV_ENV_ID), "{message}");
    }

    #[test]
    fn clap_rejects_a_bridge_command_without_required_values() {
        let result = Cli::try_parse_from(["coderex", "bridge"]);
        let error = result.unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        let message = error.to_string();
        assert!(message.contains("--control-plane-url"), "{message}");
        assert!(message.contains("--env-id"), "{message}");
    }

    #[test]
    fn bridge_help_is_available() {
        let error = Cli::try_parse_from(["coderex", "bridge", "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();
        assert!(help.contains("--control-plane-url"), "{help}");
        assert!(help.contains("--auth-token"), "{help}");
        assert!(help.contains(ENV_CONTROL_PLANE_URL), "{help}");
    }

    #[test]
    fn rejects_zero_concurrency_and_bad_heartbeat_thresholds() {
        let mut args = args_with("http://cp.example.test", "env-1");
        args.max_concurrent = 0;
        assert!(
            BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None)
                .unwrap_err()
                .to_string()
                .contains("--max-concurrent")
        );

        let mut args = args_with("http://cp.example.test", "env-1");
        args.heartbeat_lost_threshold = 1;
        args.heartbeat_degraded_threshold = 2;
        assert!(
            BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None)
                .unwrap_err()
                .to_string()
                .contains("--heartbeat-lost-threshold")
        );

        let mut args = args_with("http://cp.example.test", "env-1");
        args.poll_interval_secs = 0;
        assert!(
            BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None)
                .unwrap_err()
                .to_string()
                .contains("--poll-interval-secs")
        );

        let mut args = args_with("http://cp.example.test", "env-1");
        args.claim_path = Some("/api/claims".into());
        assert!(
            BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None)
                .unwrap_err()
                .to_string()
                .contains("{assignment_id}")
        );
    }

    #[test]
    fn claim_path_can_come_from_the_environment() {
        let mut args = args_with("http://cp.example.test", "env-1");
        args.claim_path = None;
        let config = BridgeLaunchConfig::resolve(&args, Path::new("."), |key| {
            (key == ENV_CLAIM_PATH).then(|| "/api/assignments/{assignment_id}/claim".into())
        })
        .unwrap();
        assert_eq!(config.claim_path, "/api/assignments/{assignment_id}/claim");
    }

    #[test]
    fn rejects_a_non_websocket_url_for_the_websocket_transport() {
        let mut args = args_with("unix:///run/control.sock", "env-1");
        args.transport_url = None;
        let error = BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None).unwrap_err();
        assert!(error.to_string().contains("ws:// or wss://"));
    }

    #[test]
    fn unknown_transport_fails_without_a_fallback() {
        let mut args = args_with("http://cp.example.test", "env-1");
        args.transport = "carrier-pigeon".into();
        args.transport_url = Some("ws://gateway.example.test".into());
        let config = BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None).unwrap();

        let error = match create_transport(&config) {
            Ok(_) => panic!("an unknown transport must not fall back to a real one"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("carrier-pigeon"), "{error}");
        assert!(error.contains("websocket"), "{error}");
    }

    #[tokio::test]
    async fn launcher_receives_a_frame_and_shuts_down() {
        use coderex_bridge::transport::{Transport, TransportMessage};

        let tmp = tempfile::tempdir().unwrap();
        let mut args = args_with("http://cp.example.test", "env-test");
        args.transport = "in-process".into();
        args.repo_root = Some(tmp.path().to_path_buf());
        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let control_plane = Arc::new(FakeControlPlane::default());
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                Arc::new(FixedRunner::succeeding()),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        remote
            .send(TransportMessage {
                r#type: "heartbeat".into(),
                correlation_id: Some("beat-1".into()),
                payload: "{}".into(),
                metadata: std::collections::HashMap::new(),
            })
            .await
            .unwrap();

        let reply = tokio::time::timeout(Duration::from_secs(1), remote.receive())
            .await
            .expect("the bridge should answer the heartbeat")
            .unwrap();
        assert_eq!(reply.r#type, "heartbeat_ack");
        assert_eq!(reply.correlation_id.as_deref(), Some("beat-1"));

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("the bridge should shut down")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn worker_registers_spawns_reports_capacity_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = args_with("http://cp.example.test", "env-test");
        args.transport = "in-process".into();
        args.repo_root = Some(tmp.path().to_path_buf());
        args.session_base = Some(tmp.path().join("sessions"));
        let mut config = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();
        config.poll_interval = Duration::from_millis(10);
        let session_base = config.session_base.clone();

        let control_plane = Arc::new(FakeControlPlane {
            worker_epoch: 42,
            ..FakeControlPlane::default()
        });
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(sample_assignment(&session_base));

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let worker_control_plane = Arc::clone(&control_plane);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                worker_control_plane,
                Arc::new(PendingRunner::default()),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            while control_plane.registrations.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the worker should register with the control plane");
        assert_eq!(
            control_plane.registrations.lock().unwrap().as_slice(),
            &[("env-test".to_string(), DEFAULT_MAX_CONCURRENT)]
        );

        let session_dir = session_base.join("s1");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session_dir.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the fetched assignment should be spawned");

        assert_eq!(
            control_plane.claimed(),
            vec![("env-test".to_string(), "a1".to_string())],
            "the assignment should be claimed before it is spawned"
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let reported = control_plane
                    .capacities
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|report| report.active_sessions == 1);
                if reported {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("capacity should report the active session");

        // The spawn produced a fenced write observable on the gateway side of
        // the in-process transport.
        let write = tokio::time::timeout(Duration::from_secs(2), remote.receive())
            .await
            .expect("the worker should send a session_write after spawning")
            .unwrap();
        assert_eq!(write.r#type, "session_write");
        assert_eq!(
            write.metadata.get("worker_epoch").map(String::as_str),
            Some("42"),
            "the write must carry the epoch registration returned"
        );
        let payload: serde_json::Value = serde_json::from_str(&write.payload).unwrap();
        assert_eq!(payload["session_id"], "s1");
        assert_eq!(payload["state"], "running");

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("shutdown should stop the worker")
            .unwrap();
        assert!(result.is_ok());

        // Neither loop keeps running after shutdown.
        let fetches_at_shutdown = *control_plane.fetches.lock().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            *control_plane.fetches.lock().unwrap(),
            fetches_at_shutdown,
            "assignment polling should have stopped"
        );
    }

    #[test]
    fn usage_from_result_accepts_both_api_spellings() {
        let frame = serde_json::json!({
            "type": "result",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 40
            },
            "total_cost_usd": 0.25
        });
        let usage = usage_from_result(&frame);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 30);
        assert_eq!(usage.cache_creation_tokens, 40);
        assert!((usage.cost_usd - 0.25).abs() < f64::EPSILON);

        let short = serde_json::json!({
            "usage": {
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_read_tokens": 3,
                "cache_creation_tokens": 4
            },
            "cost_usd": 0.5
        });
        let usage = usage_from_result(&short);
        assert_eq!(usage.cache_read_tokens, 3);
        assert_eq!(usage.cache_creation_tokens, 4);
        assert!((usage.cost_usd - 0.5).abs() < f64::EPSILON);
    }

    /// The shutdown grace is the default unless the override parses as a
    /// positive number of seconds.
    #[test]
    fn the_shutdown_grace_can_be_overridden() {
        assert_eq!(shutdown_grace_from(Some("30")), Duration::from_secs(30));
        assert_eq!(shutdown_grace_from(Some(" 30 ")), Duration::from_secs(30));
        for ignored in ["0", "soon", "", "  "] {
            assert_eq!(
                shutdown_grace_from(Some(ignored)),
                SESSION_SHUTDOWN_GRACE,
                "{ignored:?} should keep the default"
            );
        }
        assert_eq!(shutdown_grace_from(None), SESSION_SHUTDOWN_GRACE);
    }

    /// A session whose child never emits totals still reports what it spent:
    /// every assistant frame's own usage is summed. The result frame, when it
    /// does carry totals, is the provider's own number and wins.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_usage_falls_back_to_the_per_message_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let script = r#"printf '%s\n' '{"type":"assistant","session_id":"s1","message":{"usage":{"input_tokens":10,"output_tokens":5}}}' '{"type":"assistant","session_id":"s1","message":{"usage":{"input_tokens":7,"output_tokens":3}}}' '{"type":"result","subtype":"success","is_error":false}'"#;

        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let output = run_child_streaming(
            child,
            "s1",
            &session_dir,
            None,
            None,
            None,
            SessionStop::new(),
            Duration::from_secs(2),
            false,
        )
        .await
        .unwrap();

        assert_eq!(output.usage.input_tokens, 17);
        assert_eq!(output.usage.output_tokens, 8);
    }

    /// The result frame's totals are preferred over the sum when both exist.
    #[cfg(unix)]
    #[tokio::test]
    async fn result_totals_win_over_the_per_message_sum() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let script = r#"printf '%s\n' '{"type":"assistant","session_id":"s1","message":{"usage":{"input_tokens":10,"output_tokens":5}}}' '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":10,"output_tokens":5}}'"#;

        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let output = run_child_streaming(
            child,
            "s1",
            &session_dir,
            None,
            None,
            None,
            SessionStop::new(),
            Duration::from_secs(2),
            false,
        )
        .await
        .unwrap();

        assert_eq!(output.usage.input_tokens, 10);
        assert_eq!(output.usage.output_tokens, 5);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_runner_streams_output_and_reports_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let script = r#"printf '%s\n' '{"type":"system","subtype":"init","session_id":"s1"}' '{"type":"assistant","session_id":"s1"}' '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":11,"output_tokens":22,"cache_read_input_tokens":3,"cache_creation_input_tokens":4},"total_cost_usd":0.05}'; printf 'a warning\n' >&2"#;

        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel();
        let output = run_child_streaming(
            child,
            "s1",
            &session_dir,
            Some(output_tx),
            None,
            None,
            SessionStop::new(),
            Duration::from_secs(2),
            false,
        )
        .await
        .unwrap();

        let mut forwarded = Vec::new();
        while let Some(line) = output_rx.recv().await {
            forwarded.push(line);
        }
        assert_eq!(
            forwarded
                .iter()
                .filter(|line| line.stream == "stdout")
                .count(),
            3,
            "every stdout line should be forwarded: {forwarded:?}"
        );
        assert!(
            forwarded
                .iter()
                .any(|line| line.stream == "stderr" && line.line == "a warning"),
            "the stderr line should be forwarded: {forwarded:?}"
        );
        assert!(forwarded.iter().all(|line| line.session_id == "s1"));

        assert_eq!(output.usage.input_tokens, 11);
        assert_eq!(output.usage.output_tokens, 22);
        assert_eq!(output.usage.cache_read_tokens, 3);
        assert_eq!(output.usage.cache_creation_tokens, 4);
        assert!((output.usage.cost_usd - 0.05).abs() < f64::EPSILON);
        assert_eq!(output.transcript.lines().count(), 3);
        assert!(output.transcript.contains("\"type\":\"result\""));

        let transcript = std::fs::read_to_string(session_dir.join("transcript.jsonl")).unwrap();
        assert_eq!(transcript, output.transcript);
        let stderr = std::fs::read_to_string(session_dir.join("stderr.log")).unwrap();
        assert!(stderr.contains("a warning"), "{stderr}");
    }

    /// The full stdio round trip: the child asks, the launcher answers, and
    /// the child reads the answer off its stdin.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_permission_request_is_answered_over_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let script = r#"printf '%s\n' '{"type":"control_request","request_id":"child-req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"reason":"needs approval","allow_session":true}}'
IFS= read -r line
printf 'ECHO:%s\n' "$line""#;

        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();

        let task = tokio::spawn(async move {
            run_child_streaming(
                child,
                "sess-1",
                &session_dir,
                None,
                Some(request_tx),
                Some(decision_rx),
                SessionStop::new(),
                Duration::from_secs(5),
                false,
            )
            .await
        });

        let request = tokio::time::timeout(Duration::from_secs(5), request_rx.recv())
            .await
            .expect("the child's request should reach the launcher")
            .expect("the request channel should stay open");
        assert_eq!(request.session_id, "sess-1");
        assert_eq!(request.request_id, "child-req-1");
        assert_eq!(request.tool_name, "Bash");
        assert_eq!(request.input["command"], "ls");
        assert_eq!(request.reason, "needs approval");
        assert!(request.allow_session);

        decision_tx
            .send(ChildPermissionDecision {
                request_id: request.request_id.clone(),
                allow: true,
                allow_session: true,
                message: "approved".into(),
            })
            .unwrap();

        let output = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the child should exit once it has its answer")
            .unwrap()
            .unwrap();

        assert!(
            output.transcript.contains("\"type\":\"control_request\""),
            "the request should remain in the transcript: {}",
            output.transcript
        );
        let echoed = output
            .transcript
            .lines()
            .find_map(|line| line.strip_prefix("ECHO:"))
            .expect("the child should echo the response it read");
        let response: Value = serde_json::from_str(echoed).unwrap();
        assert_eq!(response["type"], "control_response");
        assert_eq!(response["response"]["subtype"], "success");
        assert_eq!(response["response"]["request_id"], "child-req-1");
        assert_eq!(response["response"]["response"]["behavior"], "allow");
        assert_eq!(response["response"]["response"]["allow_session"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_terminates_the_child_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("terminated");
        let script =
            "trap 'echo term > \"$1\"; exit 0' TERM; echo ready; while :; do sleep 0.05; done";
        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("sh")
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let session_dir = tmp.path().join("session");
        let stop = SessionStop::new();
        let task_stop = stop.clone();
        let handle = tokio::spawn(async move {
            run_child_streaming(
                child,
                "s1",
                &session_dir,
                None,
                None,
                None,
                task_stop,
                Duration::from_secs(2),
                false,
            )
            .await
        });

        // Give `sh` time to install the trap, then ask it to stop.
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.cancel();

        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the child should stop within the grace period")
            .unwrap();
        assert!(result.is_err(), "a cancelled session is not a success");
        assert!(
            marker.exists(),
            "the child should have run its SIGTERM handler"
        );
    }

    /// A session that leads its own process group is asked to stop through
    /// that group, so its own children get the graceful request too.
    ///
    /// This asserts the request (`kill -TERM -<pid>`) rather than the kernel's
    /// delivery: whether a grandchild's `trap` runs depends on the shell, and a
    /// test that depends on that measures `/bin/sh`, not this code. Delivery to
    /// the direct child is covered by `shutdown_terminates_the_child_gracefully`.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(unsafe_code)] // PATH mutation is unsafe in edition 2024; ENV_LOCK guards it.
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serialises PATH mutation; held across the await on purpose.
    async fn a_leading_child_is_stopped_through_its_process_group() {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("kill.log");
        // A stand-in for `kill` that records the request and exits 0.
        let fake = tmp.path().join("bin");
        std::fs::create_dir_all(&fake).unwrap();
        let kill = fake.join("kill");
        std::fs::write(
            &kill,
            format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&kill, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id().expect("child pid");

        let original_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: ENV_LOCK above keeps every other environment-touching test
        // out for the duration.
        unsafe {
            std::env::set_var("PATH", format!("{}:{original_path}", fake.display()));
        }
        let _ = terminate_child(&mut child, Duration::from_millis(100), true).await;
        unsafe {
            std::env::set_var("PATH", &original_path);
        }

        let recorded = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            recorded.lines().any(|line| line == format!("-TERM -{pid}")),
            "expected the group to be signalled, kill was asked for: {recorded:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK keeps a PATH-mutating test out for the whole run; held across the await on purpose.
    async fn shutdown_kills_a_child_that_ignores_sigterm() {
        // Both `sh` (to spawn the child) and `kill` (to probe for it) come from
        // PATH; a concurrent test that temporarily replaces PATH would make
        // them unresolvable and fail this test spuriously.
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let script = "trap '' TERM; echo ready; while :; do sleep 0.05; done";
        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().expect("child pid");

        let session_dir = tmp.path().join("session");
        let stop = SessionStop::new();
        let task_stop = stop.clone();
        let handle = tokio::spawn(async move {
            run_child_streaming(
                child,
                "s1",
                &session_dir,
                None,
                None,
                None,
                task_stop,
                Duration::from_millis(200),
                false,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.cancel();

        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the kill should be bounded")
            .unwrap();
        assert!(result.is_err());
        // The child was reaped, so `kill -0` must report that it is gone.
        let probe = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run kill -0");
        assert!(
            !probe.success(),
            "the session child should not be left running"
        );
    }

    #[test]
    fn process_runner_passes_the_prompt_as_one_headless_argument() {
        let prompt = "hello --not-a-flag\nsecond line";
        let command = session_command(prompt, Path::new(".")).unwrap();
        let std_command = command.as_std();

        assert_eq!(
            std_command.get_program(),
            std::env::current_exe().unwrap().as_os_str()
        );
        let args: Vec<String> = std_command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["--print", "--output-format", "stream-json", "--", prompt]
        );
        assert_eq!(std_command.get_current_dir(), Some(Path::new(".")));

        // The child parses the prompt as one positional argument, even when it
        // looks like a flag or spans lines, and asks for the streaming protocol.
        let parsed = Cli::try_parse_from([
            "coderex",
            "--print",
            "--output-format",
            "stream-json",
            "--",
            prompt,
        ])
        .unwrap();
        assert!(parsed.print);
        assert_eq!(parsed.output_format, "stream-json");
        assert_eq!(parsed.prompt, vec![prompt.to_string()]);
    }

    #[tokio::test]
    async fn successful_session_is_completed_and_frees_its_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let config = worker_config(tmp.path());
        let session_base = config.session_base.clone();
        let archive_base = config.archive_base.clone();

        let control_plane = Arc::new(FakeControlPlane::default());
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(sample_assignment(&session_base));
        let runner = Arc::new(FixedRunner::succeeding());
        let worker_runner = Arc::clone(&runner);
        let observed = Arc::clone(&control_plane);

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                worker_runner,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        wait_until(|| !runner.runs.lock().unwrap().is_empty(), "the runner").await;
        wait_until(
            || {
                observed
                    .capacities
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|report| report.active_sessions == 1)
            },
            "the active capacity report",
        )
        .await;
        // The next report after completion sees the freed slot.
        wait_until(
            || {
                observed
                    .capacities
                    .lock()
                    .unwrap()
                    .last()
                    .is_some_and(|report| report.active_sessions == 0)
            },
            "the slot to be freed",
        )
        .await;
        assert!(
            archive_base.join("s1/metadata.json").exists(),
            "a successful session should be archived"
        );

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("shutdown should stop the worker")
            .unwrap();
        assert!(result.is_ok());
    }

    /// A runner that starts, then waits for the test to release it.
    ///
    /// The capacity test needs the first session to *stay* running while the
    /// worker polls again; a runner that finishes immediately can free the
    /// slot before the assertion reads the claim list.
    #[derive(Debug)]
    struct GatedRunner {
        gate: tokio::sync::Semaphore,
        started: Mutex<Vec<String>>,
    }

    impl GatedRunner {
        fn new() -> Self {
            Self {
                gate: tokio::sync::Semaphore::new(0),
                started: Mutex::new(Vec::new()),
            }
        }

        fn release(&self) {
            self.gate.add_permits(1);
        }
    }

    #[async_trait::async_trait]
    impl SessionRunner for GatedRunner {
        async fn run(
            &self,
            context: &SessionContext<'_>,
            _stop: SessionStop,
        ) -> Result<SessionOutput, String> {
            self.started
                .lock()
                .unwrap()
                .push(context.session_id.to_string());
            let permit = self
                .gate
                .acquire()
                .await
                .map_err(|_| "the gate closed".to_string())?;
            permit.forget();
            Ok(SessionOutput {
                transcript: "{\"type\":\"result\",\"subtype\":\"success\"}\n".into(),
                usage: UsageStats::default(),
            })
        }
    }

    /// Capacity is enforced, not just reported: with one slot and two queued
    /// assignments, the second is not claimed until the first finishes.
    ///
    /// The poller's `take_assignment` refuses to hand out more work than
    /// `max_concurrent` allows. Without this the worker would claim an
    /// assignment, leave it unspawned, and report capacity it does not have.
    #[tokio::test]
    async fn a_full_worker_does_not_claim_a_second_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = worker_config(tmp.path());
        config.max_concurrent = 1;
        let session_base = config.session_base.clone();

        let mut first = sample_assignment(&session_base);
        first.assignment_id = "a1".into();
        first.session_id = "s1".into();
        let mut second = sample_assignment(&session_base);
        second.assignment_id = "a2".into();
        second.session_id = "s2".into();

        let control_plane = Arc::new(FakeControlPlane::default());
        control_plane
            .assignments
            .lock()
            .unwrap()
            .extend([first, second]);
        let runner = Arc::new(GatedRunner::new());
        let worker_runner = Arc::clone(&runner);
        let observed = Arc::clone(&control_plane);

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                worker_runner,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        // The first session is running and holds the only slot. Give the
        // worker a few poll intervals to (incorrectly) claim the second one.
        wait_until(
            || runner.started.lock().unwrap().len() == 1,
            "the first session to start",
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let claims_while_full = observed.claimed();
        assert_eq!(
            claims_while_full,
            vec![("env-test".to_string(), "a1".to_string())],
            "a full worker claimed more than its slots"
        );

        // Release the first session: the slot frees and the second is taken.
        runner.release();
        wait_until(
            || runner.started.lock().unwrap().len() == 2,
            "the second session after the slot freed",
        )
        .await;
        assert_eq!(observed.claimed()[1].1, "a2");

        shutdown_tx.send(()).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), running).await;
    }

    /// Session output is not only written locally: each line reaches the
    /// gateway as a `session_output` frame on the active transport.
    #[tokio::test]
    async fn session_output_is_forwarded_over_the_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let config = worker_config(tmp.path());
        let session_base = config.session_base.clone();

        let control_plane = Arc::new(FakeControlPlane::default());
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(sample_assignment(&session_base));
        let runner = Arc::new(StreamingRunner {
            lines: vec![("stdout", "hello from the child"), ("stderr", "a warning")],
        });

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                runner,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        let mut forwarded = Vec::new();
        while forwarded.len() < 2 {
            let frame = tokio::time::timeout(Duration::from_secs(2), remote.receive())
                .await
                .expect("the session output should reach the transport")
                .unwrap();
            if frame.r#type == "session_output" {
                let payload: serde_json::Value = serde_json::from_str(&frame.payload).unwrap();
                forwarded.push(payload);
            }
        }

        assert_eq!(forwarded[0]["session_id"], "s1");
        assert_eq!(forwarded[0]["stream"], "stdout");
        assert_eq!(forwarded[0]["line"], "hello from the child");
        assert_eq!(forwarded[1]["stream"], "stderr");
        assert_eq!(forwarded[1]["line"], "a warning");

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("shutdown should stop the worker")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn failed_session_fails_and_frees_its_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let config = worker_config(tmp.path());
        let session_base = config.session_base.clone();
        let archive_base = config.archive_base.clone();

        let control_plane = Arc::new(FakeControlPlane::default());
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(sample_assignment(&session_base));
        let runner = Arc::new(FixedRunner::failing("exit code 7: boom"));
        let worker_runner = Arc::clone(&runner);
        let observed = Arc::clone(&control_plane);

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                worker_runner,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        wait_until(|| !runner.runs.lock().unwrap().is_empty(), "the runner").await;
        wait_until(
            || {
                observed
                    .capacities
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|report| report.active_sessions == 1)
            },
            "the active capacity report",
        )
        .await;
        wait_until(
            || {
                observed
                    .capacities
                    .lock()
                    .unwrap()
                    .last()
                    .is_some_and(|report| report.active_sessions == 0)
            },
            "the failed slot to be freed",
        )
        .await;
        assert!(
            !archive_base.join("s1/metadata.json").exists(),
            "a failed session should not be archived as completed"
        );

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("shutdown should stop the worker")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn lost_claim_skips_the_session_and_frees_the_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = worker_config(tmp.path());
        // With one slot, the second assignment can only run if the lost claim
        // released the slot taken for the first.
        config.max_concurrent = 1;
        let session_base = config.session_base.clone();

        let control_plane = Arc::new(FakeControlPlane::default());
        control_plane.queue_claim_outcomes(vec![Ok(ClaimOutcome::Lost), Ok(ClaimOutcome::Claimed)]);
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(sample_assignment(&session_base));
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(WorkAssignment {
                assignment_id: "a2".into(),
                session_id: "s2".into(),
                working_directory: session_base.display().to_string(),
                prompt: "Second".into(),
                spawn_mode: SpawnMode::SingleSession,
            });

        let runner = Arc::new(FixedRunner::succeeding());
        let worker_runner = Arc::clone(&runner);
        let observed = Arc::clone(&control_plane);

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                worker_runner,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        wait_until(
            || observed.claimed().len() >= 2,
            "both assignments to be claimed",
        )
        .await;
        wait_until(
            || !runner.runs.lock().unwrap().is_empty(),
            "the second session to run",
        )
        .await;

        // The first assignment was skipped; only the second spawned.
        assert_eq!(
            observed.claimed(),
            vec![
                ("env-test".to_string(), "a1".to_string()),
                ("env-test".to_string(), "a2".to_string())
            ]
        );
        assert_eq!(runner.runs.lock().unwrap().as_slice(), &["s2".to_string()]);
        assert!(!session_base.join("s1").exists());

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("shutdown should stop the worker")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn failed_claim_fails_the_session_and_frees_the_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = worker_config(tmp.path());
        config.max_concurrent = 1;
        let session_base = config.session_base.clone();

        let control_plane = Arc::new(FakeControlPlane::default());
        control_plane.queue_claim_outcomes(vec![
            Err("control plane unavailable".into()),
            Ok(ClaimOutcome::Claimed),
        ]);
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(sample_assignment(&session_base));
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(WorkAssignment {
                assignment_id: "a2".into(),
                session_id: "s2".into(),
                working_directory: session_base.display().to_string(),
                prompt: "Second".into(),
                spawn_mode: SpawnMode::SingleSession,
            });

        let runner = Arc::new(FixedRunner::succeeding());
        let worker_runner = Arc::clone(&runner);
        let observed = Arc::clone(&control_plane);

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                worker_runner,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        wait_until(
            || observed.claimed().len() >= 2,
            "both assignments to be claimed",
        )
        .await;
        wait_until(
            || !runner.runs.lock().unwrap().is_empty(),
            "the second session to run",
        )
        .await;

        // The failed claim spawned nothing and freed its slot for `s2`.
        assert_eq!(runner.runs.lock().unwrap().as_slice(), &["s2".to_string()]);
        assert!(!session_base.join("s1").exists());

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("shutdown should stop the worker")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shutdown_cancels_a_running_session() {
        let tmp = tempfile::tempdir().unwrap();
        let config = worker_config(tmp.path());
        let session_base = config.session_base.clone();

        let control_plane = Arc::new(FakeControlPlane::default());
        control_plane
            .assignments
            .lock()
            .unwrap()
            .push(sample_assignment(&session_base));
        let runner = Arc::new(PendingRunner::default());
        let worker_runner = Arc::clone(&runner);

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_control_plane(
                config,
                Box::new(bridge_transport),
                control_plane,
                worker_runner,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), runner.started.notified())
            .await
            .expect("the session runner should start");

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("shutdown should stop the worker promptly")
            .unwrap();
        assert!(result.is_ok());
        assert!(
            runner.cancelled.load(Ordering::SeqCst),
            "shutdown should cancel the running session task"
        );
    }

    #[tokio::test]
    async fn launcher_reports_a_connect_failure() {
        use async_trait::async_trait;
        use coderex_bridge::transport::{TransportError, TransportMessage};

        #[derive(Debug)]
        struct RefusingTransport;

        #[async_trait]
        impl Transport for RefusingTransport {
            async fn connect(&mut self) -> Result<(), TransportError> {
                Err(TransportError::ConnectionLost("refused by test".into()))
            }

            async fn disconnect(&mut self) -> Result<(), TransportError> {
                Ok(())
            }

            fn is_connected(&self) -> bool {
                false
            }

            async fn send(&mut self, _: TransportMessage) -> Result<(), TransportError> {
                Err(TransportError::NotConnected)
            }

            async fn receive(&mut self) -> Result<TransportMessage, TransportError> {
                Err(TransportError::NotConnected)
            }

            fn connection_id(&self) -> &'static str {
                "refusing"
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let mut args = args_with("http://cp.example.test", "env-test");
        args.transport = "in-process".into();
        args.repo_root = Some(tmp.path().to_path_buf());
        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();

        let error = run_with_control_plane(
            config,
            Box::new(RefusingTransport),
            Arc::new(FakeControlPlane::default()),
            Arc::new(FixedRunner::succeeding()),
            async {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("could not connect"), "{error}");
        assert!(error.contains("refused by test"), "{error}");
    }

    // ─── Permission timeouts and the missing lifecycle states ───

    async fn permission_test_bridge(tmp: &Path, epoch: u64) -> Bridge {
        let mut config = worker_config(tmp).bridge_config();
        config.archive_base = tmp.join("archives");
        let (transport, _remote) =
            InProcessTransport::create_pair("bridge-perm".into(), "gateway-perm".into());
        let mut bridge = Bridge::with_control_plane(
            config,
            Box::new(transport),
            Arc::new(FakeControlPlane {
                worker_epoch: epoch,
                ..FakeControlPlane::default()
            }),
        );
        bridge.connect().await.unwrap();
        if epoch != 0 {
            bridge.register().await.unwrap();
        }
        bridge
    }

    async fn spawn_permission_session(bridge: &mut Bridge, tmp: &Path) {
        std::fs::create_dir_all(tmp.join("sessions")).unwrap();
        let assignment = sample_assignment(&tmp.join("sessions"));
        bridge.spawn_session(&assignment).await.unwrap();
        bridge.mark_session_running("s1").unwrap();
    }

    fn running_permission_session() -> (
        HashMap<String, RunningSession>,
        tokio::sync::mpsc::UnboundedReceiver<ChildPermissionDecision>,
    ) {
        let (permission_tx, decisions) =
            tokio::sync::mpsc::unbounded_channel::<ChildPermissionDecision>();
        let mut running = HashMap::new();
        running.insert(
            "s1".to_string(),
            RunningSession {
                handle: tokio::spawn(async {}),
                stop: SessionStop::new(),
                permission_tx,
            },
        );
        (running, decisions)
    }

    fn child_request(request_id: &str) -> ChildPermissionRequest {
        ChildPermissionRequest {
            session_id: "s1".into(),
            request_id: request_id.into(),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "rm -rf /tmp/x" }),
            reason: "needs approval".into(),
            allow_session: false,
        }
    }

    #[tokio::test]
    async fn an_unanswered_child_permission_times_out_and_denies() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = permission_test_bridge(tmp.path(), 0).await;
        spawn_permission_session(&mut bridge, tmp.path()).await;
        let (running, mut decisions) = running_permission_session();

        let (timeout_tx, mut timeout_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut tracker = PermissionTracker::new(Some(Duration::from_millis(50)), timeout_tx);

        tracker
            .register(&mut bridge, &running, child_request("child-req-timeout"))
            .await;
        assert_eq!(tracker.len(), 1);
        assert_eq!(
            bridge.session_state("s1"),
            Some(&BridgeSessionState::AwaitingPermission)
        );

        let request_id = tokio::time::timeout(Duration::from_secs(2), timeout_rx.recv())
            .await
            .expect("the bounded wait should expire")
            .expect("the timeout channel should stay open");
        assert_eq!(request_id, "child-req-timeout");

        tracker.expire(&mut bridge, &request_id);

        let decision = decisions
            .recv()
            .await
            .expect("the child should receive a decision");
        assert!(!decision.allow);
        assert!(
            decision.message.contains("timed out"),
            "the message must report the timeout, not a denial by the user: {}",
            decision.message
        );
        assert_eq!(tracker.len(), 0, "the pending map is empty");
        assert_eq!(
            bridge.session_state("s1"),
            Some(&BridgeSessionState::Running),
            "the session leaves AwaitingPermission after its last request"
        );
        assert!(matches!(
            bridge.get_permission_response("child-req-timeout"),
            Some(PermissionResponse::Deny)
        ));

        // A gateway answer that arrives after the deadline cannot resolve the
        // dead request or overwrite the denial the child already received.
        assert!(!bridge.respond_to_permission(
            "child-req-timeout",
            PermissionResponse::Allow { always: true }
        ));
        assert!(matches!(
            bridge.get_permission_response("child-req-timeout"),
            Some(PermissionResponse::Deny)
        ));
    }

    #[tokio::test]
    async fn a_timely_child_permission_answer_beats_the_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = permission_test_bridge(tmp.path(), 0).await;
        spawn_permission_session(&mut bridge, tmp.path()).await;
        let (running, mut decisions) = running_permission_session();

        let (timeout_tx, mut timeout_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut tracker = PermissionTracker::new(Some(Duration::from_millis(50)), timeout_tx);

        tracker
            .register(&mut bridge, &running, child_request("child-req-answered"))
            .await;
        assert_eq!(tracker.len(), 1);

        assert!(bridge.respond_to_permission(
            "child-req-answered",
            PermissionResponse::Allow { always: true }
        ));
        tracker.deliver(&mut bridge, "child-req-answered");

        let decision = decisions
            .recv()
            .await
            .expect("the child should receive the answer");
        assert!(decision.allow);
        assert!(decision.allow_session);
        assert_eq!(tracker.len(), 0);
        assert_eq!(
            bridge.session_state("s1"),
            Some(&BridgeSessionState::Running)
        );

        // Dropping the pending entry stops the bounded wait: no timeout fires
        // after the answer.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            timeout_rx.try_recv().is_err(),
            "an answered request must not report a timeout"
        );
    }

    /// A permission answer in the shape the gateway sends it back.
    fn permission_answer(request_id: &str, allowed: bool) -> TransportMessage {
        TransportMessage {
            r#type: "permission_response".into(),
            correlation_id: Some(request_id.to_string()),
            payload: serde_json::json!({
                "request_id": request_id,
                "allowed": allowed,
                "always": false,
            })
            .to_string(),
            metadata: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn a_child_permission_round_trips_through_the_relay_exactly_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = worker_config(tmp.path()).bridge_config();
        config.archive_base = tmp.path().join("archives");
        let (transport, mut gateway) =
            InProcessTransport::create_pair("bridge-perm".into(), "gateway-perm".into());
        gateway.connect().await.unwrap();
        let mut bridge = Bridge::with_control_plane(
            config,
            Box::new(transport),
            Arc::new(FakeControlPlane::default()),
        );
        bridge.connect().await.unwrap();
        spawn_permission_session(&mut bridge, tmp.path()).await;
        let (running, mut decisions) = running_permission_session();

        // No timeout: the relay is what decides whether an answer may resolve.
        let (timeout_tx, _timeout_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut tracker = PermissionTracker::new(None, timeout_tx);
        let mut events = bridge.subscribe();

        // The child asks; the launcher registers it under the child's own id
        // and flushes it over the transport.
        tracker
            .register(&mut bridge, &running, child_request("child-req-1"))
            .await;

        // It reaches the gateway as a `permission_request`, carrying the
        // child's id and the session it belongs to.
        let request = tokio::time::timeout(Duration::from_secs(2), gateway.receive())
            .await
            .expect("the request reaches the gateway")
            .unwrap();
        assert_eq!(request.r#type, "permission_request");
        assert_eq!(request.correlation_id.as_deref(), Some("child-req-1"));
        let payload: Value = serde_json::from_str(&request.payload).unwrap();
        assert_eq!(payload["request_id"], "child-req-1");
        assert_eq!(payload["session_id"], "s1");

        // The gateway answers; the bridge relays it, resolves the router and
        // broadcasts the resolution, which the launcher turns into a decision.
        let reply = bridge
            .handle_transport_message(&permission_answer("child-req-1", true))
            .unwrap();
        assert_eq!(reply.r#type, "permission_result");
        let resolved = loop {
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("a resolution is broadcast")
                .expect("the event channel stays open");
            if let BridgeEvent::PermissionResolved { request_id, .. } = event {
                break request_id;
            }
        };
        assert_eq!(resolved, "child-req-1");
        tracker.deliver(&mut bridge, &resolved);

        let decision = decisions
            .recv()
            .await
            .expect("the child receives its answer");
        assert_eq!(decision.request_id, "child-req-1");
        assert!(decision.allow);
        assert!(
            decisions.try_recv().is_err(),
            "the child must receive exactly one answer"
        );

        // A replay of the same answer is refused by the relay: it resolves
        // nothing and never reaches the child a second time.
        let replay = bridge
            .handle_transport_message(&permission_answer("child-req-1", true))
            .unwrap();
        let replay_payload: Value = serde_json::from_str(&replay.payload).unwrap();
        assert_eq!(replay_payload["resolved"], false);
        assert!(
            events.try_recv().is_err(),
            "a duplicate must not broadcast a second resolution"
        );
        assert!(
            decisions.try_recv().is_err(),
            "a duplicate must not reach the child"
        );
    }

    #[tokio::test]
    async fn launcher_drives_each_missing_state_and_refuses_invalid_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = permission_test_bridge(tmp.path(), 0).await;
        spawn_permission_session(&mut bridge, tmp.path()).await;

        // The spawner's sequence for a directory session: Preparing then
        // Spawning, never CreatingWorktree.
        assert_eq!(
            bridge.sessions.get("s1").unwrap().state_machine.history,
            vec![
                BridgeSessionState::Assigned,
                BridgeSessionState::Preparing,
                BridgeSessionState::Spawning,
                BridgeSessionState::Running,
            ]
        );

        let (running, mut decisions) = running_permission_session();
        let (timeout_tx, _timeout_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut tracker = PermissionTracker::new(None, timeout_tx);
        tracker
            .register(&mut bridge, &running, child_request("child-req-state"))
            .await;
        assert_eq!(
            bridge.session_state("s1"),
            Some(&BridgeSessionState::AwaitingPermission)
        );

        // A waiting session cannot skip the return to Running.
        assert!(
            bridge
                .sessions
                .get_mut("s1")
                .unwrap()
                .state_machine
                .transition(BridgeSessionState::Archiving)
                .is_err()
        );

        assert!(bridge.respond_to_permission("child-req-state", PermissionResponse::Deny));
        tracker.deliver(&mut bridge, "child-req-state");
        assert!(decisions.recv().await.is_some());
        assert_eq!(
            bridge.session_state("s1"),
            Some(&BridgeSessionState::Running)
        );

        // A failed runner leaves the session in Failed.
        finish_session(
            &mut bridge,
            SessionCompletion {
                session_id: "s1".into(),
                outcome: Err("child exited 1".into()),
            },
        )
        .await;
        assert!(matches!(
            bridge.sessions.get("s1").unwrap().state_machine.current,
            BridgeSessionState::Failed(ref reason) if reason == "child exited 1"
        ));
    }

    #[tokio::test]
    async fn finish_session_reports_completed_for_a_successful_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = permission_test_bridge(tmp.path(), 9).await;
        spawn_permission_session(&mut bridge, tmp.path()).await;

        finish_session(
            &mut bridge,
            SessionCompletion {
                session_id: "s1".into(),
                outcome: Ok(SessionOutput {
                    transcript: "{\"type\":\"result\",\"subtype\":\"success\"}\n".into(),
                    usage: UsageStats::default(),
                }),
            },
        )
        .await;

        assert!(
            !bridge.sessions.contains_key("s1"),
            "a completed session is removed from tracking"
        );
        let frames = bridge.take_pending_session_writes();
        let states: Vec<String> = frames
            .iter()
            .map(|frame| {
                let payload: Value = serde_json::from_str(&frame.payload).unwrap();
                payload["state"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            states,
            vec!["completed".to_string()],
            "the final write is Completed, not Failed"
        );
    }

    /// Build a real, long-lived session child command, as the runner would.
    #[cfg(unix)]
    fn long_lived_child() -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Whether `pid` names a live process, via the POSIX `kill -0` probe the
    /// workspace uses elsewhere instead of `libc::kill`.
    #[cfg(unix)]
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run kill -0")
            .success()
    }

    /// A session child spawned through the runner's registration path is
    /// registered with the bridge and killed by the cleanup that ends the
    /// session — not merely forgotten.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_spawned_session_process_is_registered_and_killed_by_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = permission_test_bridge(tmp.path(), 0).await;
        spawn_permission_session(&mut bridge, tmp.path()).await;

        // The registry the bridge's cleanup reads from.
        let registry = bridge.process_registry();

        let mut command = long_lived_child();
        let (child, tree) = spawn_registered_child(&mut command, "s1", Some(&registry))
            .expect("the session child should spawn");
        let pid = child.id().expect("the child should have a pid");
        assert!(tree.is_some(), "a registered child owns a process tree");
        assert!(
            registry.contains("s1"),
            "the spawned session child must be registered for cleanup"
        );
        assert!(
            process_is_alive(pid),
            "the session child should be running before cleanup"
        );

        // Drive the child the way the worker's runner does, so this is a real
        // running session rather than a bare process.
        let session_dir = tmp.path().join("sessions/s1");
        let handle = tokio::spawn(async move {
            run_child_streaming(
                child,
                "s1",
                &session_dir,
                None,
                None,
                None,
                SessionStop::new(),
                Duration::from_secs(5),
                false,
            )
            .await
        });

        // Ending the session runs cleanup, which must terminate the process it
        // registered.
        bridge
            .complete_session("s1", "", UsageStats::default())
            .await
            .unwrap();
        assert!(
            !registry.contains("s1"),
            "cleanup deregisters the session's process"
        );

        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the killed child should stop the reader")
            .unwrap();
        assert!(outcome.is_err(), "a killed session is not a success");

        // The process is gone, not merely removed from the map.
        assert!(
            !process_is_alive(pid),
            "the session child should not be left running after cleanup"
        );
    }

    /// A failed session never reaches `complete_session`, so it must be
    /// deregistered directly or the registry would grow without bound.
    #[cfg(unix)]
    #[tokio::test]
    async fn fail_session_deregisters_the_session_process() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = permission_test_bridge(tmp.path(), 0).await;
        spawn_permission_session(&mut bridge, tmp.path()).await;
        let registry = bridge.process_registry();

        let mut command = long_lived_child();
        let (mut child, _tree) = spawn_registered_child(&mut command, "s1", Some(&registry))
            .expect("the session child should spawn");
        assert!(registry.contains("s1"));

        bridge.fail_session("s1", "child exited 1".into());

        assert!(
            !registry.contains("s1"),
            "a failed session must be deregistered"
        );

        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}
