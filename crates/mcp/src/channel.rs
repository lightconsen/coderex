//! JSON-RPC channel over stdio — subprocess management and message exchange.

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

/// A channel for JSON-RPC communication over stdio with a subprocess.
#[derive(Debug)]
pub struct JsonRpcChannel {
    /// The child process.
    process: Option<Child>,
    /// Stdin handle for writing requests.
    stdin: Option<ChildStdin>,
    /// Stdout reader for reading responses.
    stdout: Option<BufReader<ChildStdout>>,
    /// Next request ID counter.
    next_id: AtomicU64,
    /// Server command.
    command: String,
    /// Server arguments.
    args: Vec<String>,
    /// Environment variables.
    env: HashMap<String, String>,
    /// Whether the channel is initialized.
    initialized: bool,
}

impl JsonRpcChannel {
    /// Create a new channel (not yet spawned).
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            process: None,
            stdin: None,
            stdout: None,
            next_id: AtomicU64::new(1),
            command: command.into(),
            args,
            env,
            initialized: false,
        }
    }

    /// Spawn the subprocess and capture stdio handles.
    pub async fn spawn(&mut self) -> Result<(), String> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args);
        cmd.envs(&self.env);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut process = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn '{}': {e}", self.command))?;

        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| "failed to take stdin from child process".to_string())?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| "failed to take stdout from child process".to_string())?;

        self.process = Some(process);
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));
        Ok(())
    }

    /// Perform the MCP initialize/initialized handshake.
    pub async fn initialize(&mut self) -> Result<Value, String> {
        // Send initialize request
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "coderex",
                "version": "0.1.0"
            }
        });

        let result = self.send_request("initialize", Some(init_params)).await?;

        // Send initialized notification (no response expected)
        self.send_notification("initialized", None).await?;
        self.initialized = true;

        Ok(result)
    }

    /// Discover tools from the MCP server.
    pub async fn list_tools(&mut self) -> Result<Value, String> {
        self.send_request("tools/list", None).await
    }

    /// Discover resources from the MCP server.
    pub async fn list_resources(&mut self) -> Result<Value, String> {
        self.send_request("resources/list", None).await
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

    /// Write a request to stdin.
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

    /// Write a notification to stdin.
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

    /// Read a response from stdout matching the given ID.
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

    /// Check if the channel is alive by attempting to read from stdout.
    pub async fn ping(&mut self) -> Result<(), String> {
        // For MCP servers, we check if the process is still running
        if let Some(ref mut process) = self.process {
            match process.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!("process exited with status: {status}"));
                }
                Ok(None) => {} // Still running
                Err(e) => {
                    return Err(format!("failed to check process status: {e}"));
                }
            }
        } else {
            return Err("process not spawned".to_string());
        }

        Ok(())
    }

    /// Check if initialized.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Check if the process is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.process.is_some()
    }

    /// Terminate the process.
    pub async fn terminate(&mut self) -> Result<(), String> {
        if let Some(ref mut process) = self.process {
            process
                .kill()
                .await
                .map_err(|e| format!("failed to kill process: {e}"))?;
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
    async fn new_channel_not_initialized() {
        let channel = JsonRpcChannel::new("echo", vec![], HashMap::new());
        assert!(!channel.is_initialized());
        assert!(!channel.is_running());
    }

    #[tokio::test]
    async fn spawn_valid_process() {
        // Use a command that stays alive briefly (cat reads from stdin)
        let mut channel = JsonRpcChannel::new("cat", vec![], HashMap::new());
        let result = channel.spawn().await;
        assert!(result.is_ok(), "spawn failed: {result:?}");
        assert!(channel.is_running());
        // Clean up
        let _ = channel.terminate().await;
    }

    #[tokio::test]
    async fn spawn_invalid_command_fails() {
        let mut channel = JsonRpcChannel::new("nonexistent_command_xyz", vec![], HashMap::new());
        let result = channel.spawn().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ping_before_spawn_fails() {
        let mut channel = JsonRpcChannel::new("echo", vec![], HashMap::new());
        let result = channel.ping().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ping_after_spawn_succeeds() {
        let mut channel = JsonRpcChannel::new("cat", vec![], HashMap::new());
        channel.spawn().await.unwrap();
        let result = channel.ping().await;
        assert!(result.is_ok());
        let _ = channel.terminate().await;
    }

    #[tokio::test]
    async fn terminate_clears_state() {
        let mut channel = JsonRpcChannel::new("cat", vec![], HashMap::new());
        channel.spawn().await.unwrap();
        assert!(channel.is_running());
        channel.terminate().await.unwrap();
        assert!(!channel.is_running());
        assert!(!channel.is_initialized());
    }

    #[tokio::test]
    async fn request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 42,
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({"name": "read_file"})),
        };
        let serialized = serde_json::to_string(&req).unwrap();
        let parsed: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "tools/call");
    }

    #[tokio::test]
    async fn response_parsing() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hello"}]}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 1);
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[tokio::test]
    async fn error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(response.error.is_some());
        let err = response.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }
}
