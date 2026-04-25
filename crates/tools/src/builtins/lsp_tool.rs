//! LSP tool — language server protocol client for symbol resolution.
//!
//! Supports go-to-definition, find-references, hover, and workspace symbols
//! by communicating with a language server process via JSON-RPC over stdio.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::Value;
use url::Url;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Language server operation to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspOperation {
    /// Search workspace symbols.
    WorkspaceSymbol,
    /// Get hover information at a position.
    Hover,
    /// Go to definition at a position.
    Definition,
    /// Find all references at a position.
    References,
}

impl std::str::FromStr for LspOperation {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "workspace_symbols" => Ok(Self::WorkspaceSymbol),
            "hover" => Ok(Self::Hover),
            "definition" => Ok(Self::Definition),
            "references" => Ok(Self::References),
            _ => Err(format!("unknown operation: {s}")),
        }
    }
}

/// Connection to a language server process.
#[derive(Debug)]
pub struct LspConnection {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: u64,
}

impl LspConnection {
    /// Spawn a language server process and initialize the connection.
    pub fn spawn(command: &str, args: &[&str], root_uri: &str) -> Result<Self, ToolError> {
        let mut process = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                ToolError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to spawn language server: {e}"),
                ))
            })?;

        let stdin = process.stdin.take().ok_or_else(|| {
            ToolError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "failed to capture stdin".to_string(),
            ))
        })?;

        let stdout = process.stdout.take().ok_or_else(|| {
            ToolError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "failed to capture stdout".to_string(),
            ))
        })?;

        let mut conn = Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
        };

        conn.initialize(root_uri)?;
        Ok(conn)
    }

    /// Perform the LSP initialize handshake.
    fn initialize(&mut self, root_uri: &str) -> Result<(), ToolError> {
        let uri = Url::parse(root_uri).map_err(|e| ToolError::InvalidInput {
            message: format!("invalid root URI: {e}"),
        })?;

        let params = serde_json::json!({
            "processId": null,
            "workspaceFolders": [{
                "uri": uri.to_string(),
                "name": uri.path()
            }],
            "capabilities": {}
        });

        let response = self.send_request_raw("initialize", params)?;
        let _server_info = response.get("serverInfo");

        self.send_notification("initialized", serde_json::json!({}))?;
        Ok(())
    }

    /// Send a JSON-RPC request and return the result as JSON.
    fn send_request_raw(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        self.request_id += 1;
        let id = self.request_id;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.write_message(&request)?;
        let response = self.read_response()?;

        if let Some(error) = response.get("error") {
            return Err(ToolError::InvalidInput {
                message: format!("LSP error: {error}"),
            });
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send a JSON-RPC notification.
    fn send_notification(&mut self, method: &str, params: Value) -> Result<(), ToolError> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&notification)
    }

    /// Write a JSON-RPC message with LSP headers.
    fn write_message(&mut self, msg: &Value) -> Result<(), ToolError> {
        let body = serde_json::to_string(msg).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to serialize: {e}"),
        })?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin
            .write_all(header.as_bytes())
            .map_err(ToolError::Io)?;
        self.stdin
            .write_all(body.as_bytes())
            .map_err(ToolError::Io)?;
        self.stdin.flush().map_err(ToolError::Io)
    }

    /// Read a JSON-RPC response from the server.
    fn read_response(&mut self) -> Result<Value, ToolError> {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).map_err(ToolError::Io)?;
            if n == 0 {
                return Err(ToolError::InvalidInput {
                    message: "server closed connection".to_string(),
                });
            }
            let line = line.trim().to_string();
            if line.is_empty() {
                break;
            }
            if let Some(content_len) = line.strip_prefix("Content-Length: ") {
                let len: usize = content_len.parse().map_err(|e| ToolError::InvalidInput {
                    message: format!("invalid Content-Length: {e}"),
                })?;

                // Read the body bytes
                let mut buf = vec![0u8; len];
                let mut pos = 0;
                while pos < len {
                    let bytes_read = self.stdout.read(&mut buf[pos..]).map_err(ToolError::Io)?;
                    if bytes_read == 0 {
                        break;
                    }
                    pos += bytes_read;
                }
                let body = String::from_utf8_lossy(&buf);
                return serde_json::from_str(&body).map_err(|e| ToolError::InvalidInput {
                    message: format!("failed to parse response: {e}"),
                });
            }
        }
        Err(ToolError::InvalidInput {
            message: "no Content-Length header in response".to_string(),
        })
    }

    /// Terminate the language server process.
    pub fn terminate(&mut self) {
        let _ = self.process.kill();
    }
}

/// LSP tool — connects to a language server and performs symbol operations.
#[derive(Debug)]
pub struct LspTool;

#[async_trait]
impl Tool for LspTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "LSP".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["operation"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["workspace_symbols", "hover", "definition", "references"],
                        "description": "LSP operation to perform"
                    },
                    "query": { "type": "string", "description": "Symbol search query (for workspace_symbols)" },
                    "file": { "type": "string", "description": "File URI (for hover/definition/references)" },
                    "line": { "type": "number", "description": "Line number (0-based, for hover/definition/references)" },
                    "column": { "type": "number", "description": "Column number (0-based, for hover/definition/references)" },
                    "server_command": { "type": "string", "description": "Language server command (default: typescript-language-server)" },
                    "server_args": { "type": "array", "items": { "type": "string" }, "description": "Language server arguments" },
                    "root_uri": { "type": "string", "description": "Project root URI" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" },
                    "count": { "type": "number" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let operation_str = extract_string(&input.arguments, "operation")?;
        let operation = operation_str
            .parse::<LspOperation>()
            .map_err(|e| ToolError::InvalidInput { message: e })?;

        let server_cmd = extract_string(&input.arguments, "server_command")
            .unwrap_or_else(|_| "typescript-language-server".into());
        let server_args: Vec<String> = input.arguments["server_args"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["--stdio".into()]);
        let root_uri = extract_string(&input.arguments, "root_uri")
            .unwrap_or_else(|_| format!("file://{}", ctx.cwd.display()));

        let args: Vec<&str> = server_args.iter().map(|s| s.as_str()).collect();

        let mut conn = LspConnection::spawn(&server_cmd, &args, &root_uri)?;

        let result = match operation {
            LspOperation::WorkspaceSymbol => self.workspace_symbols(&mut conn, &input.arguments),
            LspOperation::Hover => self.hover(&mut conn, &input.arguments),
            LspOperation::Definition => self.definition(&mut conn, &input.arguments),
            LspOperation::References => self.references(&mut conn, &input.arguments),
        };

        conn.terminate();
        result
    }
}

impl LspTool {
    fn workspace_symbols(
        &self,
        conn: &mut LspConnection,
        args: &Value,
    ) -> Result<ToolOutput, ToolError> {
        let query = extract_string(args, "query").unwrap_or_default();

        let params = serde_json::json!({
            "query": query,
        });

        let response = conn.send_request_raw("workspace/symbol", params)?;

        let symbols = response.as_array().map_or(vec![], |arr| {
            arr.iter()
                .filter_map(|s| {
                    let name = s["name"].as_str().unwrap_or("?").to_string();
                    let kind = s["kind"]
                        .as_u64()
                        .map(|k| format!("kind:{k}"))
                        .unwrap_or_default();
                    let loc = &s["location"];
                    let uri = loc["uri"].as_str().unwrap_or("?");
                    let start_line = loc["range"]["start"]["line"].as_u64().map_or(0, |l| l + 1);
                    let start_col = loc["range"]["start"]["character"]
                        .as_u64()
                        .map_or(0, |c| c + 1);
                    Some(format!(
                        "  {name} ({kind}) — {uri}:{start_line}:{start_col}"
                    ))
                })
                .collect()
        });

        let count = symbols.len();
        let mut output = format!("Found {count} symbol(s):\n");
        for line in &symbols {
            output.push_str(line);
            output.push('\n');
        }

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }

    fn hover(&self, conn: &mut LspConnection, args: &Value) -> Result<ToolOutput, ToolError> {
        let file = extract_string(args, "file")?;
        let line = extract_number(args, "line").unwrap_or(0) as u32;
        let column = extract_number(args, "column").unwrap_or(0) as u32;

        let params = serde_json::json!({
            "textDocument": { "uri": file },
            "position": { "line": line, "character": column }
        });

        let response = conn.send_request_raw("textDocument/hover", params)?;

        if response.is_null() {
            return Ok(ToolOutput {
                output: "No hover information available".into(),
                is_display: true,
            });
        }

        let contents = &response["contents"];
        let text = if let Some(value) = contents["value"].as_str() {
            value.to_string()
        } else if let Some(value) = contents.as_str() {
            value.to_string()
        } else if let Some(arr) = contents.as_array() {
            arr.iter()
                .filter_map(|v| {
                    v.get("value")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .or_else(|| v.as_str().map(String::from))
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            response.to_string()
        };

        Ok(ToolOutput {
            output: text,
            is_display: true,
        })
    }

    fn definition(&self, conn: &mut LspConnection, args: &Value) -> Result<ToolOutput, ToolError> {
        let file = extract_string(args, "file")?;
        let line = extract_number(args, "line").unwrap_or(0) as u32;
        let column = extract_number(args, "column").unwrap_or(0) as u32;

        let params = serde_json::json!({
            "textDocument": { "uri": file },
            "position": { "line": line, "character": column }
        });

        let response = conn.send_request_raw("textDocument/definition", params)?;

        let locations: Vec<(&str, u64, u64)> = if let Some(arr) = response.as_array() {
            arr.iter()
                .filter_map(|loc| {
                    let uri = loc["uri"].as_str()?;
                    let start_line = loc["range"]["start"]["line"].as_u64()?;
                    let start_col = loc["range"]["start"]["character"].as_u64()?;
                    Some((uri, start_line + 1, start_col + 1))
                })
                .collect()
        } else if let Some(uri) = response["uri"].as_str() {
            let start_line = response["range"]["start"]["line"].as_u64().unwrap_or(0);
            let start_col = response["range"]["start"]["character"]
                .as_u64()
                .unwrap_or(0);
            vec![(uri, start_line + 1, start_col + 1)]
        } else {
            vec![]
        };

        let count = locations.len();
        let mut output = format!("Found {count} definition(s):\n");
        for (uri, line, col) in &locations {
            output.push_str(&format!("  {uri}:{line}:{col}\n"));
        }

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }

    fn references(&self, conn: &mut LspConnection, args: &Value) -> Result<ToolOutput, ToolError> {
        let file = extract_string(args, "file")?;
        let line = extract_number(args, "line").unwrap_or(0) as u32;
        let column = extract_number(args, "column").unwrap_or(0) as u32;

        let params = serde_json::json!({
            "textDocument": { "uri": file },
            "position": { "line": line, "character": column },
            "context": { "includeDeclaration": true }
        });

        let response = conn.send_request_raw("textDocument/references", params)?;

        let locations: Vec<(&str, u64, u64)> = response.as_array().map_or(vec![], |arr| {
            arr.iter()
                .filter_map(|loc| {
                    let uri = loc["uri"].as_str()?;
                    let start_line = loc["range"]["start"]["line"].as_u64()?;
                    let start_col = loc["range"]["start"]["character"].as_u64()?;
                    Some((uri, start_line + 1, start_col + 1))
                })
                .collect()
        });

        let count = locations.len();
        let mut output = format!("Found {count} reference(s):\n");
        for (uri, line, col) in &locations {
            output.push_str(&format!("  {uri}:{line}:{col}\n"));
        }

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }
}

fn extract_string(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

fn extract_number(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    #[test]
    fn descriptor_is_feature_gated() {
        let desc = LspTool.descriptor();
        assert_eq!(desc.name, "LSP");
        assert!(matches!(desc.tier, ToolTier::FeatureGated));
        assert!(desc.is_read_only);
        assert!(!desc.is_concurrency_safe);
    }

    #[test]
    fn workspace_symbol_input_schema() {
        let schema = LspTool.descriptor().input_schema;
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("operation"));
        assert!(props.contains_key("query"));
        assert!(props.contains_key("file"));
        assert!(props.contains_key("server_command"));
        assert!(props.contains_key("root_uri"));
    }

    #[test]
    fn extract_string_errors() {
        assert!(extract_string(&serde_json::json!({}), "key").is_err());
        assert!(extract_string(&serde_json::json!({ "key": 123 }), "key").is_err());
    }

    #[test]
    fn lsp_operation_parsing() {
        assert_eq!(
            "workspace_symbols".parse::<LspOperation>().unwrap(),
            LspOperation::WorkspaceSymbol
        );
        assert!("unknown".parse::<LspOperation>().is_err());
    }

    #[tokio::test]
    async fn lsp_tool_errors_without_server() {
        let ctx = test_ctx(Path::new("/tmp"));
        let result = LspTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "LSP".into(),
                    arguments: serde_json::json!({
                        "operation": "workspace_symbols",
                        "query": "foo"
                    }),
                },
                ctx,
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn extract_number_parses_various_values() {
        assert_eq!(
            extract_number(&serde_json::json!({ "line": 0 }), "line"),
            Some(0)
        );
        assert_eq!(
            extract_number(&serde_json::json!({ "line": 42 }), "line"),
            Some(42)
        );
        assert_eq!(extract_number(&serde_json::json!({}), "line"), None);
        assert_eq!(
            extract_number(&serde_json::json!({ "line": "abc" }), "line"),
            None
        );
    }
}
