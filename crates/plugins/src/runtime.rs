//! Plugin process runtime — JSON-RPC stdio transport for external process plugins.
//!
//! Spawns plugin executables and communicates via JSON-RPC 2.0 over stdio,
//! using the `coderex/` method namespace.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// A JSON-RPC request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC version.
    pub jsonrpc: String,
    /// Unique request ID.
    pub id: u64,
    /// Method name.
    pub method: String,
    /// Parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC version.
    pub jsonrpc: String,
    /// Matching request ID.
    pub id: u64,
    /// Result (if successful).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error (if failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code.
    pub code: i64,
    /// Error message.
    pub message: String,
    /// Optional extra data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC notification (no ID, no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// JSON-RPC version.
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Description of a command exposed by a plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginCommandInfo {
    /// Command name.
    pub name: String,
    /// Command description.
    pub description: String,
    /// Argument spec (optional).
    #[serde(default)]
    pub args: Vec<String>,
}

/// Description of a skill exposed by a plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginSkillInfo {
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Hooks this skill responds to.
    #[serde(default)]
    pub hooks: Vec<String>,
}

/// A running plugin process with JSON-RPC stdio communication.
#[derive(Debug)]
pub struct PluginProcess {
    /// The child process.
    process: Option<Child>,
    /// Stdin handle for writing requests.
    stdin: Option<ChildStdin>,
    /// Stdout reader for reading responses.
    stdout: Option<BufReader<ChildStdout>>,
    /// Next request ID counter.
    next_id: AtomicU64,
    /// Command to spawn.
    command: String,
    /// Arguments for the command.
    args: Vec<String>,
    /// Environment variables.
    env: HashMap<String, String>,
    /// Working directory for the plugin.
    cwd: std::path::PathBuf,
    /// Whether the channel is initialized.
    initialized: bool,
    /// Discovered commands from the plugin.
    commands: Vec<PluginCommandInfo>,
    /// Discovered skills from the plugin.
    skills: Vec<PluginSkillInfo>,
}

impl PluginProcess {
    /// Create a new plugin process (not yet spawned).
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self {
            process: None,
            stdin: None,
            stdout: None,
            next_id: AtomicU64::new(1),
            command: command.into(),
            args,
            env,
            cwd,
            initialized: false,
            commands: Vec::new(),
            skills: Vec::new(),
        }
    }

    /// Spawn the subprocess and capture stdio handles.
    pub fn spawn(&mut self) -> Result<(), String> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args);
        cmd.envs(&self.env);
        cmd.current_dir(&self.cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut process = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn plugin '{}': {e}", self.command))?;

        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| "failed to take stdin from plugin process".to_string())?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| "failed to take stdout from plugin process".to_string())?;

        self.process = Some(process);
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));
        Ok(())
    }

    /// Perform the coderex initialize handshake.
    pub async fn initialize(&mut self) -> Result<Value, String> {
        let init_params = serde_json::json!({
            "protocolVersion": "1.0",
            "capabilities": {},
            "clientInfo": {
                "name": "coderex",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self
            .send_request("coderex/initialize", Some(init_params))
            .await?;
        self.initialized = true;
        Ok(result)
    }

    /// Discover commands from the plugin.
    pub async fn list_commands(&mut self) -> Result<Vec<PluginCommandInfo>, String> {
        let result = self.send_request("coderex/commands/list", None).await?;
        let commands: Vec<PluginCommandInfo> = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse commands/list response: {e}"))?;
        self.commands.clone_from(&commands);
        Ok(commands)
    }

    /// Execute a plugin command.
    pub async fn execute_command(&mut self, name: &str, args: &[String]) -> Result<String, String> {
        let params = serde_json::json!({
            "name": name,
            "args": args,
        });
        let result = self
            .send_request("coderex/commands/execute", Some(params))
            .await?;
        match result {
            Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    /// Discover skills from the plugin.
    pub async fn list_skills(&mut self) -> Result<Vec<PluginSkillInfo>, String> {
        let result = self.send_request("coderex/skills/list", None).await?;
        let skills: Vec<PluginSkillInfo> = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse skills/list response: {e}"))?;
        self.skills.clone_from(&skills);
        Ok(skills)
    }

    /// Execute a plugin skill.
    pub async fn execute_skill(&mut self, name: &str, context: &Value) -> Result<String, String> {
        let params = serde_json::json!({
            "name": name,
            "context": context,
        });
        let result = self
            .send_request("coderex/skills/execute", Some(params))
            .await?;
        match result {
            Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    /// Ping the plugin process for health.
    pub async fn ping(&mut self) -> Result<(), String> {
        self.send_request("coderex/ping", None).await?;
        Ok(())
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn send_request(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        self.write_request(&request).await?;
        self.read_response(id).await
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn send_notification(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), String> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };
        self.write_notification(&notification).await
    }

    async fn write_request(&mut self, request: &JsonRpcRequest) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin not available")?;
        let line = serde_json::to_string(&request)
            .map_err(|e| format!("failed to serialize request: {e}"))?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("failed to write to stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("failed to flush stdin: {e}"))?;
        Ok(())
    }

    async fn write_notification(
        &mut self,
        notification: &JsonRpcNotification,
    ) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin not available")?;
        let line = serde_json::to_string(&notification)
            .map_err(|e| format!("failed to serialize notification: {e}"))?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("failed to write to stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("failed to flush stdin: {e}"))?;
        Ok(())
    }

    async fn read_response(&mut self, expected_id: u64) -> Result<Value, String> {
        let stdout = self.stdout.as_mut().ok_or("stdout not available")?;
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = stdout
                .read_line(&mut line)
                .await
                .map_err(|e| format!("failed to read from stdout: {e}"))?;

            if bytes_read == 0 {
                return Err("stdout closed before response".to_string());
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let response: JsonRpcResponse = serde_json::from_str(trimmed)
                .map_err(|e| format!("failed to parse JSON-RPC response: {e}, raw: {trimmed}"))?;

            if response.id == expected_id {
                if let Some(error) = response.error {
                    return Err(format!(
                        "JSON-RPC error (code {}): {}",
                        error.code, error.message
                    ));
                }
                return Ok(response.result.unwrap_or(Value::Null));
            }
            // If ID doesn't match, it might be a notification — continue reading
        }
    }

    /// Check if initialized.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Check if the process is still running.
    ///
    /// `try_wait` also reaps an exited child. A plugin that died between
    /// requests must not keep reporting `Active`.
    pub fn is_running(&mut self) -> bool {
        match self.process.as_mut() {
            Some(process) => matches!(process.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Get the discovered commands.
    #[must_use]
    pub fn commands(&self) -> &[PluginCommandInfo] {
        &self.commands
    }

    /// Get the discovered skills.
    #[must_use]
    pub fn skills(&self) -> &[PluginSkillInfo] {
        &self.skills
    }

    /// Terminate the process gracefully.
    pub async fn terminate(&mut self) -> Result<(), String> {
        // Try graceful shutdown request first
        let _ = self.send_notification("coderex/shutdown", None).await;

        if let Some(ref mut process) = self.process {
            process
                .kill()
                .await
                .map_err(|e| format!("failed to kill plugin process: {e}"))?;
            let _ = process.wait().await;
        }
        self.process = None;
        self.stdin = None;
        self.stdout = None;
        self.initialized = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_process_not_initialized() {
        let mut process = PluginProcess::new(
            "echo",
            vec![],
            HashMap::new(),
            std::path::PathBuf::from("/"),
        );
        assert!(!process.is_initialized());
        assert!(!process.is_running());
    }

    #[tokio::test]
    async fn spawn_valid_process() {
        let mut process =
            PluginProcess::new("cat", vec![], HashMap::new(), std::path::PathBuf::from("/"));
        let result = process.spawn();
        assert!(result.is_ok(), "spawn failed: {result:?}");
        assert!(process.is_running());
        let _ = process.terminate().await;
    }

    #[tokio::test]
    async fn spawn_invalid_command_fails() {
        let mut process = PluginProcess::new(
            "nonexistent_command_xyz",
            vec![],
            HashMap::new(),
            std::path::PathBuf::from("/"),
        );
        let result = process.spawn();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn terminate_clears_state() {
        let mut process =
            PluginProcess::new("cat", vec![], HashMap::new(), std::path::PathBuf::from("/"));
        process.spawn().unwrap();
        assert!(process.is_running());
        process.terminate().await.unwrap();
        assert!(!process.is_running());
        assert!(!process.is_initialized());
    }

    #[test]
    fn request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 42,
            method: "coderex/ping".to_string(),
            params: Some(serde_json::json!({})),
        };
        let serialized = serde_json::to_string(&req).unwrap();
        let parsed: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "coderex/ping");
    }

    #[test]
    fn response_parsing() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"commands":[]}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 1);
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[test]
    fn error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(response.error.is_some());
        let err = response.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_that_exited_is_not_running() {
        let mut process = PluginProcess::new(
            "sh",
            vec!["-c".into(), "exit 7".into()],
            std::collections::HashMap::new(),
            std::env::temp_dir(),
        );
        process.spawn().expect("spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process.is_running() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!process.is_running(), "the exited child still looked alive");
    }
}
