//! Hook execution engine — configuration, matching, and subprocess execution.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::events::{HookAction, HookError, HookEvent, HookResult};
use coderex_core::hook_executor::{HookExecutor, HookFireResult};

// ─── Hook Configuration ─────────────────────────────────────────────────────

/// Configuration for a single hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Event(s) this hook responds to.
    pub event: String,
    /// Command to execute.
    pub command: String,
    /// Optional environment variables to inject.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional working directory for the hook.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Whether this is an internal hook (not user-configured).
    #[serde(default)]
    pub internal: bool,
    /// Timeout in milliseconds (default 5000).
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Execution priority (higher runs first, default 0).
    #[serde(default)]
    pub priority: u32,
    /// If true, execute but ignore stop decisions (for testing).
    #[serde(default)]
    pub dry_run: bool,
    /// Maximum CPU seconds for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    /// Maximum memory (MB) for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Maximum output file size (MB) for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
}

const fn default_timeout() -> u64 {
    5000
}

/// Build a shell command wrapper that applies resource limits via `ulimit`.
///
/// Returns `None` if no limits are configured.
fn sandbox_prefix(config: &HookConfig) -> Option<String> {
    let mut parts = Vec::new();

    // RLIMIT_CPU: max CPU time in seconds (-t)
    if let Some(cpu_secs) = config.max_cpu_seconds {
        parts.push(format!("ulimit -t {cpu_secs}"));
    }

    // RLIMIT_FSIZE: max file size in KB (-f), convert MB → KB
    if let Some(size_mb) = config.max_file_size_mb {
        let size_kb = size_mb * 1024;
        parts.push(format!("ulimit -f {size_kb}"));
    }

    // RLIMIT_AS (virtual memory): max in KB (-v), convert MB → KB
    if let Some(mem_mb) = config.max_memory_mb {
        let mem_kb = mem_mb * 1024;
        parts.push(format!("ulimit -v {mem_kb}"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

// ─── Hook Settings (from settings.json) ─────────────────────────────────────

/// Parsed hook settings from the `hooks` field in settings.json.
#[derive(Debug, Clone, Deserialize)]
pub struct HookSettings {
    /// List of hook configurations.
    #[serde(default)]
    pub hooks: Vec<RawHookConfig>,
}

/// Raw hook config as it appears in settings.json (more flexible field types).
#[derive(Debug, Clone, Deserialize)]
pub struct RawHookConfig {
    /// Event name or pattern.
    pub event: String,
    /// Command to run.
    pub command: String,
    /// Environment variables to inject.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Whether internal.
    #[serde(default)]
    pub internal: Option<bool>,
    /// Timeout in ms.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Execution priority.
    #[serde(default)]
    pub priority: Option<u32>,
    /// Dry-run mode.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Max CPU seconds.
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    /// Max memory MB.
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Max file size MB.
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
}

// ─── Hook Execution Result ──────────────────────────────────────────────────

/// Result from executing a hook, including subprocess output.
#[derive(Debug, Clone)]
pub struct HookExecutionResult {
    /// Parsed hook result from stdout.
    pub hook_result: HookResult,
    /// Standard output from the hook subprocess.
    pub stdout: String,
    /// Standard error from the hook subprocess.
    pub stderr: String,
    /// Exit code from the hook subprocess.
    pub exit_code: i32,
}

// ─── Hook Input (serialized to stdin) ───────────────────────────────────────

/// Input sent to a hook subprocess via stdin as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct HookInput {
    /// The event type that triggered this hook.
    pub event: String,
    /// Session ID.
    pub session_id: Option<String>,
    /// Current user input.
    pub input: Option<String>,
    /// Additional context about the event.
    pub context: Option<String>,
}

// ─── Hook Registry ──────────────────────────────────────────────────────────

/// Registry of configured hooks.
#[derive(Debug)]
pub struct HookRegistry {
    hooks: Vec<HookConfig>,
}

impl HookRegistry {
    /// Create an empty hook registry.
    #[must_use]
    pub const fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Load hooks from settings.json value.
    pub fn load_from_settings(value: &serde_json::Value) -> Result<Self, HookError> {
        if value.is_null() {
            return Ok(Self::new());
        }
        let settings: HookSettings = serde_json::from_value(value.clone())?;
        let hooks = settings
            .hooks
            .into_iter()
            .map(|raw| HookConfig {
                event: raw.event,
                command: raw.command,
                env: raw.env.unwrap_or_default(),
                cwd: raw.cwd,
                internal: raw.internal.unwrap_or(false),
                timeout_ms: raw.timeout_ms.unwrap_or_else(default_timeout),
                priority: raw.priority.unwrap_or(0),
                dry_run: raw.dry_run.unwrap_or(false),
                max_cpu_seconds: raw.max_cpu_seconds,
                max_memory_mb: raw.max_memory_mb,
                max_file_size_mb: raw.max_file_size_mb,
            })
            .collect();
        Ok(Self { hooks })
    }

    /// Register a hook configuration.
    pub fn register(&mut self, config: HookConfig) {
        self.hooks.push(config);
    }

    /// Find hooks matching the given event, sorted by priority (highest first).
    #[must_use]
    pub fn matches_for_event(&self, event: &HookEvent) -> Vec<&HookConfig> {
        let event_name = event.as_str();
        let mut hooks: Vec<_> = self
            .hooks
            .iter()
            .filter(|h| h.event == event_name || h.event == "*")
            .collect();
        hooks.sort_by_key(|h| std::cmp::Reverse(h.priority));
        hooks
    }

    /// Find hooks matching the given event name string, sorted by priority (highest first).
    fn matches_for_event_name(&self, event_name: &str) -> Vec<&HookConfig> {
        let mut hooks: Vec<_> = self
            .hooks
            .iter()
            .filter(|h| h.event == event_name || h.event == "*")
            .collect();
        hooks.sort_by_key(|h| std::cmp::Reverse(h.priority));
        hooks
    }

    /// Execute matching hooks synchronously for a named event.
    async fn execute_sync_for_event(
        &self,
        event_name: &str,
        input: &HookInput,
    ) -> Vec<Result<HookExecutionResult, HookError>> {
        let matching = self.matches_for_event_name(event_name);
        let mut results = Vec::new();
        for hook in matching {
            let result = execute_hook_with_timeout(hook, input).await;
            results.push(result);
        }
        results
    }

    /// Execute matching hooks synchronously (sequential, collect results).
    pub async fn execute_sync(
        &self,
        event: &HookEvent,
        input: &HookInput,
    ) -> Vec<Result<HookExecutionResult, HookError>> {
        let matching = self.matches_for_event(event);
        let mut results = Vec::new();
        for hook in matching {
            let result = execute_hook_with_timeout(hook, input).await;
            results.push(result);
        }
        results
    }

    /// Execute matching hooks asynchronously (spawn tasks, collect results).
    pub async fn execute_async(
        &self,
        event: &HookEvent,
        input: &HookInput,
    ) -> Vec<Result<HookExecutionResult, HookError>> {
        let matching: Vec<_> = self.matches_for_event(event);
        let mut handles = Vec::new();
        for hook in matching {
            let hook = hook.clone();
            let input = input.clone();
            let handle =
                tokio::spawn(async move { execute_hook_with_timeout(&hook, &input).await });
            handles.push(handle);
        }
        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(Err(HookError::SpawnFailed(e.to_string()))),
            }
        }
        results
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl HookExecutor for HookRegistry {
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult {
        // Build hook input
        let hook_input = HookInput {
            event: event_name.to_string(),
            session_id: context.get("session_id").cloned(),
            input: Some(input.to_string()),
            context: None,
        };

        // Execute matching hooks for this event
        let results = self.execute_sync_for_event(event_name, &hook_input).await;

        let mut combined = HookFireResult::default();
        for exec_result in results.into_iter() {
            match exec_result {
                Ok(r) => {
                    if matches!(r.hook_result.action, HookAction::Stop) {
                        combined.stop = true;
                    }
                    if r.hook_result.updated_input.is_some() {
                        combined.updated_input = r.hook_result.updated_input;
                    }
                }
                Err(e) => {
                    combined.errors.push(format!("{e}"));
                }
            }
        }
        combined
    }
}

// ─── Hook Execution ─────────────────────────────────────────────────────────

/// Execute a single hook with timeout.
async fn execute_hook_with_timeout(
    hook: &HookConfig,
    input: &HookInput,
) -> Result<HookExecutionResult, HookError> {
    let start = std::time::Instant::now();

    if hook.dry_run {
        tracing::info!(
            hook_command = hook.command,
            event = input.event,
            "Hook would execute (dry-run)"
        );
    }

    let timeout_ms = hook.timeout_ms;
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        execute_hook(hook, input),
    )
    .await
    .map_or(Err(HookError::Timeout(timeout_ms)), |r| r);

    let elapsed = start.elapsed();

    match &result {
        Ok(exec_result) => {
            tracing::info!(
                hook_command = hook.command,
                event = input.event,
                duration_ms = elapsed.as_millis(),
                exit_code = exec_result.exit_code,
                stop = matches!(exec_result.hook_result.action, HookAction::Stop),
                "Hook executed"
            );
        }
        Err(e) => {
            tracing::warn!(
                hook_command = hook.command,
                event = input.event,
                duration_ms = elapsed.as_millis(),
                error = ?e,
                "Hook failed"
            );
        }
    }

    // In dry-run mode, suppress stop actions so they don't affect execution.
    if hook.dry_run {
        if let Ok(mut exec_result) = result {
            if matches!(exec_result.hook_result.action, HookAction::Stop) {
                exec_result.hook_result.action = HookAction::Continue;
                exec_result.hook_result.suppress_output = false;
            }
            return Ok(exec_result);
        }
    }

    result
}

/// Execute a single hook subprocess.
async fn execute_hook(
    hook: &HookConfig,
    input: &HookInput,
) -> Result<HookExecutionResult, HookError> {
    let input_json = serde_json::to_string(input)?;
    let env = build_hook_env(hook, input);

    // Wrap command with sandbox limits if configured.
    let shell_cmd = match sandbox_prefix(hook) {
        Some(prefix) => format!("{prefix}; {command}", command = hook.command),
        None => hook.command.clone(),
    };

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&shell_cmd)
        .envs(&env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| HookError::SpawnFailed(e.to_string()))?;

    // Write input to stdin (ignore broken pipe — hook may not read stdin)
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(input_json.as_bytes()).await {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(HookError::SpawnFailed(e.to_string()));
            }
        }
    }

    // Collect stdout and stderr
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| HookError::SpawnFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    let hook_result = if exit_code == 0 && !stdout.trim().is_empty() {
        parse_hook_output(&stdout).unwrap_or_else(|_| default_continue_result())
    } else {
        default_continue_result()
    };

    Ok(HookExecutionResult {
        hook_result,
        stdout,
        stderr,
        exit_code,
    })
}

/// Parse a HookResult from a hook's stdout (single JSON line).
fn parse_hook_output(stdout: &str) -> Result<HookResult, HookError> {
    let line = stdout.lines().next().ok_or(HookError::ExecutionFailed(0))?;
    parse_hook_output_line(line)
}

/// Parse a single JSON line into a HookResult.
pub fn parse_hook_output_line(line: &str) -> Result<HookResult, HookError> {
    let parsed: serde_json::Value = serde_json::from_str(line)?;

    let action = parsed
        .get("action")
        .and_then(|v| v.as_str())
        .map_or(HookAction::Continue, |s| match s {
            "stop" => HookAction::Stop,
            _ => HookAction::Continue,
        });

    let suppress_output = parsed
        .get("suppress_output")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let updated_input = parsed
        .get("updated_input")
        .and_then(|v| v.as_str())
        .map(String::from);

    let additional_context = parsed
        .get("additional_context")
        .and_then(|v| v.as_str())
        .map(String::from);

    let watch_paths = parsed
        .get("watch_paths")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let permission_decision = parsed.get("permission_decision").and_then(|v| {
        serde_json::from_value::<crate::events::HookPermissionDecision>(v.clone()).ok()
    });

    Ok(HookResult {
        action,
        suppress_output,
        updated_input,
        additional_context,
        watch_paths,
        permission_decision,
    })
}

/// Default HookResult: Continue, no suppression.
const fn default_continue_result() -> HookResult {
    HookResult {
        action: HookAction::Continue,
        suppress_output: false,
        updated_input: None,
        additional_context: None,
        watch_paths: Vec::new(),
        permission_decision: None,
    }
}

/// Build the environment map for a hook subprocess.
fn build_hook_env(hook: &HookConfig, input: &HookInput) -> HashMap<String, String> {
    let mut env = std::env::vars().collect::<HashMap<_, _>>();

    // Inject hook metadata
    env.insert("CODEREX_HOOK_EVENT".to_string(), input.event.clone());
    if let Some(ref session_id) = input.session_id {
        env.insert("CODEREX_SESSION_ID".to_string(), session_id.clone());
    }

    // Inject hook-specific env
    for (key, value) in &hook.env {
        env.insert(key.clone(), value.clone());
    }

    env
}

/// Helper to spawn a hook process (exposed for testing).
#[cfg(test)]
pub fn spawn_hook_process(command: &str, _input: &str) -> std::process::Child {
    use std::process::{Command as StdCommand, Stdio};
    StdCommand::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn test hook process")
}

// ─── Hook Matcher ───────────────────────────────────────────────────────────

/// Matcher for finding hooks by event.
#[derive(Debug)]
pub struct HookMatcher<'a> {
    registry: &'a HookRegistry,
}

impl<'a> HookMatcher<'a> {
    /// Create a new matcher from a registry.
    #[must_use]
    pub const fn new(registry: &'a HookRegistry) -> Self {
        Self { registry }
    }

    /// Find all hooks matching the given event.
    #[must_use]
    pub fn matches(&self, event: &HookEvent) -> Vec<&'a HookConfig> {
        self.registry.matches_for_event(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_registry_starts_empty() {
        let registry = HookRegistry::new();
        assert!(
            registry
                .matches_for_event(&HookEvent::SessionStart)
                .is_empty()
        );
    }

    #[test]
    fn register_and_match_single_hook() {
        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "SessionStart".to_string(),
            command: "echo hello".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        });

        let matches = registry.matches_for_event(&HookEvent::SessionStart);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command, "echo hello");
    }

    #[test]
    fn wildcard_hook_matches_all_events() {
        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "*".to_string(),
            command: "echo all".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        });

        assert_eq!(
            registry.matches_for_event(&HookEvent::SessionStart).len(),
            1
        );
        assert_eq!(registry.matches_for_event(&HookEvent::Stop).len(), 1);
    }

    #[test]
    fn load_from_settings_parses_hooks() {
        let value = serde_json::json!({
            "hooks": [
                {
                    "event": "PreToolUse",
                    "command": "echo pre",
                    "env": {"FOO": "bar"}
                }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn load_from_settings_null_returns_empty() {
        let registry = HookRegistry::load_from_settings(&serde_json::Value::Null).unwrap();
        assert!(
            registry
                .matches_for_event(&HookEvent::SessionStart)
                .is_empty()
        );
    }

    #[test]
    fn load_from_settings_invalid_json_errors() {
        let value = serde_json::json!({"hooks": "not an array"});
        let result = HookRegistry::load_from_settings(&value);
        assert!(result.is_err());
    }

    #[test]
    fn parse_hook_output_line_continue() {
        let line = r#"{"action": "continue", "suppress_output": false}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(matches!(result.action, HookAction::Continue));
        assert!(!result.suppress_output);
    }

    #[test]
    fn parse_hook_output_line_stop() {
        let line = r#"{"action": "stop", "suppress_output": true}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(matches!(result.action, HookAction::Stop));
        assert!(result.suppress_output);
    }

    #[test]
    fn parse_hook_output_line_updated_input() {
        let line = r#"{"action": "continue", "updated_input": "new prompt"}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert_eq!(result.updated_input, Some("new prompt".to_string()));
    }

    #[test]
    fn parse_hook_output_line_watch_paths() {
        let line = r#"{"action": "continue", "watch_paths": ["/src", "/lib"]}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert_eq!(result.watch_paths, vec!["/src", "/lib"]);
    }

    #[test]
    fn parse_hook_output_line_permission_decision() {
        let line = r#"{"action": "continue", "permission_decision": {"allow": true, "allow_for_path": null, "reason": "safe"}}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(result.permission_decision.is_some());
        let decision = result.permission_decision.unwrap();
        assert!(decision.allow);
    }

    #[test]
    fn parse_hook_output_line_missing_fields_defaults() {
        let line = r#"{}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(matches!(result.action, HookAction::Continue));
        assert!(!result.suppress_output);
        assert!(result.updated_input.is_none());
        assert!(result.watch_paths.is_empty());
    }

    #[test]
    fn parse_hook_output_line_invalid_json() {
        let line = "not json";
        let result = parse_hook_output_line(line);
        assert!(matches!(result, Err(HookError::JsonError(_))));
    }

    #[test]
    fn hook_matcher_matches_registered_hooks() {
        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "PreToolUse".to_string(),
            command: "echo".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        });
        registry.register(HookConfig {
            event: "*".to_string(),
            command: "echo all".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        });

        let matcher = HookMatcher::new(&registry);
        let matches = matcher.matches(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn hook_matcher_no_match_for_unregistered_event() {
        let registry = HookRegistry::new();
        let matcher = HookMatcher::new(&registry);
        assert!(matcher.matches(&HookEvent::Stop).is_empty());
    }

    #[test]
    fn build_hook_env_includes_metadata() {
        let hook = HookConfig {
            event: "SessionStart".to_string(),
            command: "echo".to_string(),
            env: [("MY_VAR".to_string(), "my_val".to_string())]
                .into_iter()
                .collect(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        let input = HookInput {
            event: "SessionStart".to_string(),
            session_id: Some("sess_123".to_string()),
            input: None,
            context: None,
        };
        let env = build_hook_env(&hook, &input);
        assert_eq!(env.get("CODEREX_HOOK_EVENT").unwrap(), "SessionStart");
        assert_eq!(env.get("CODEREX_SESSION_ID").unwrap(), "sess_123");
        assert_eq!(env.get("MY_VAR").unwrap(), "my_val");
    }

    #[test]
    fn event_as_str_maps_correctly() {
        assert_eq!(HookEvent::PreToolUse.as_str(), "PreToolUse");
        assert_eq!(HookEvent::PostToolUseFailure.as_str(), "PostToolUseFailure");
        assert_eq!(HookEvent::PostCompact.as_str(), "PostCompact");
        assert_eq!(HookEvent::Stop.as_str(), "Stop");
    }

    #[test]
    fn event_name_round_trips_and_accepts_legacy_alias() {
        use std::str::FromStr;
        for event in <HookEvent as strum::IntoEnumIterator>::iter() {
            assert_eq!(HookEvent::from_str(event.as_str()), Ok(event));
        }
        // 历史写法仍可解析到同一事件
        assert_eq!(
            HookEvent::from_str("PostToolUseFail"),
            Ok(HookEvent::PostToolUseFailure)
        );
        assert!(HookEvent::from_str("NoSuchEvent").is_err());
    }

    #[tokio::test]
    async fn execute_hook_returns_continue_for_simple_command() {
        let hook = HookConfig {
            event: "SessionStart".to_string(),
            command: "echo hello".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        let input = HookInput {
            event: "SessionStart".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input).await.unwrap();
        assert!(matches!(result.hook_result.action, HookAction::Continue));
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn execute_hook_timeout_kills_process() {
        let hook = HookConfig {
            event: "Stop".to_string(),
            command: "sleep 10".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 50,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        let input = HookInput {
            event: "Stop".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input).await;
        assert!(matches!(result, Err(HookError::Timeout(50))));
    }

    #[test]
    fn sandbox_prefix_none_when_no_limits() {
        let hook = HookConfig {
            event: "Test".to_string(),
            command: "echo test".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        assert!(sandbox_prefix(&hook).is_none());
    }

    #[test]
    fn sandbox_prefix_with_limits() {
        let hook = HookConfig {
            event: "Test".to_string(),
            command: "echo test".to_string(),
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: Some(10),
            max_memory_mb: Some(256),
            max_file_size_mb: Some(50),
        };
        let prefix = sandbox_prefix(&hook).unwrap();
        assert!(prefix.contains("ulimit -t 10"));
        assert!(prefix.contains("ulimit -v 262144")); // 256 * 1024
        assert!(prefix.contains("ulimit -f 51200")); // 50 * 1024
    }
}
