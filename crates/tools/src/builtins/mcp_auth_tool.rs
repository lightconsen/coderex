//! McpAuth tool — trigger MCP OAuth authentication flow.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::time::Duration;

/// Local redirect URI used to capture the OAuth callback.
const LOCAL_REDIRECT_URI: &str = "http://localhost:8765/callback";

/// Trigger MCP OAuth authentication flow for a server.
#[derive(Debug)]
pub struct McpAuthTool;

#[async_trait]
impl Tool for McpAuthTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "McpAuth".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["server_url"],
                "properties": {
                    "server_url": { "type": "string", "description": "MCP server URL to authenticate with" },
                    "client_id": { "type": "string", "description": "OAuth client ID (default: coderex-cli)" },
                    "scope": { "type": "string", "description": "OAuth scope (default: mcp)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "authenticated": { "type": "boolean" },
                    "auth_url": { "type": "string" },
                    "access_token": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let server_url = extract_string(&input.arguments, "server_url")?;
        let client_id =
            extract_string(&input.arguments, "client_id").unwrap_or_else(|_| "coderex-cli".into());
        let scope = extract_string(&input.arguments, "scope").unwrap_or_else(|_| "mcp".into());

        // Generate a random state parameter
        let state = format!("coderex-{}", uuid::Uuid::new_v4());

        // Build authorization URL
        let auth_url = format!(
            "{server_url}/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope={}&state={}",
            url_encode(LOCAL_REDIRECT_URI),
            url_encode(&scope),
            url_encode(&state),
        );

        // Try to open the browser
        let _ = open_browser(&auth_url);

        // Spawn local HTTP listener to capture callback
        #[cfg(test)]
        let callback_timeout = Duration::from_secs(2);
        #[cfg(not(test))]
        let callback_timeout = Duration::from_secs(300);
        let code = match wait_for_callback(&state, callback_timeout).await {
            Ok(code) => code,
            Err(e) => {
                return Ok(ToolOutput {
                    output: serde_json::json!({
                        "authenticated": false,
                        "auth_url": auth_url,
                        "note": format!("Waiting for OAuth callback timed out or failed: {e}. Please visit auth_url manually and provide the code."),
                    }).to_string(),
                    is_display: true,
                });
            }
        };

        // Exchange code for token
        let token_url = format!("{server_url}/oauth/token");
        let client = reqwest::Client::new();
        let resp = client
            .post(&token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client_id),
                ("redirect_uri", LOCAL_REDIRECT_URI),
            ])
            .send()
            .await
            .map_err(|e| ToolError::InvalidInput {
                message: format!("token request failed: {e}"),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::InvalidInput {
                message: format!("token endpoint returned HTTP {status}: {body}"),
            });
        }

        let token_resp: serde_json::Value =
            resp.json().await.map_err(|e| ToolError::InvalidInput {
                message: format!("failed to parse token response: {e}"),
            })?;

        let access_token = token_resp
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput {
                message: "token response missing access_token".into(),
            })?;

        // Persist token locally
        let expires_in = token_resp
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let refresh_token = token_resp
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from);
        let _ = persist_token(
            &server_url,
            &client_id,
            access_token,
            expires_in,
            refresh_token.as_deref(),
        );

        Ok(ToolOutput {
            output: serde_json::json!({
                "authenticated": true,
                "auth_url": auth_url,
                "access_token": access_token,
            })
            .to_string(),
            is_display: true,
        })
    }
}

/// Wait for an OAuth callback on localhost:8765 that matches the expected state.
async fn wait_for_callback(expected_state: &str, timeout: Duration) -> Result<String, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8765")
        .await
        .map_err(|e| format!("failed to bind callback listener: {e}"))?;

    let result = tokio::time::timeout(timeout, async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("accept failed: {e}"))?;

            let mut buf = [0u8; 2048];
            let n = stream
                .peek(&mut buf)
                .await
                .map_err(|e| format!("peek failed: {e}"))?;
            if n == 0 {
                continue;
            }
            let req = String::from_utf8_lossy(&buf[..n]);

            // Extract query string from GET line: GET /callback?code=...
            let Some(line) = req.lines().next() else {
                continue;
            };
            let Some(query_start) = line.find('?') else {
                // Send error page and continue waiting
                let _ = send_http_response(&mut stream, 400, "Missing query parameters").await;
                continue;
            };
            let query_end = line.find(' ').unwrap_or(line.len());
            let query = &line[query_start + 1..query_end];

            let params: std::collections::HashMap<&str, &str> =
                query.split('&').filter_map(|p| p.split_once('=')).collect();

            let received_state = params.get("state").copied().unwrap_or("");
            if received_state != expected_state {
                let _ = send_http_response(&mut stream, 400, "Invalid state parameter").await;
                continue;
            }

            let Some(code) = params.get("code").copied() else {
                let _ = send_http_response(&mut stream, 400, "Missing authorization code").await;
                continue;
            };

            let _ = send_http_response(
                &mut stream,
                200,
                "Authentication successful! You can close this window.",
            )
            .await;

            return Ok::<String, String>(code.to_string());
        }
    })
    .await;

    match result {
        Ok(Ok(code)) => Ok(code),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("OAuth callback timed out".into()),
    }
}

async fn send_http_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    use tokio::io::AsyncWriteExt;
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()?;
    }
    Ok(())
}

fn url_encode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

/// Persist an MCP token to `~/.coderex/mcp_tokens.json`.
fn persist_token(
    server_url: &str,
    client_id: &str,
    access_token: &str,
    expires_in: i64,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    let path = home::home_dir()
        .unwrap_or_default()
        .join(".coderex")
        .join("mcp_tokens.json");

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    struct TokenEntry {
        server_url: String,
        client_id: String,
        access_token: String,
        refresh_token: Option<String>,
        expires_at: i64,
    }

    let mut entries: Vec<TokenEntry> = if path.exists() {
        let content = std::fs::read_to_string(&path).map_err(|e| format!("read: {e}"))?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Remove existing entry for this server
    entries.retain(|e| e.server_url != server_url);

    let expires_at = chrono::Utc::now().timestamp() + expires_in;
    entries.push(TokenEntry {
        server_url: server_url.to_string(),
        client_id: client_id.to_string(),
        access_token: access_token.to_string(),
        refresh_token: refresh_token.map(String::from),
        expires_at,
    });

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content = serde_json::to_string_pretty(&entries).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&path, content).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn mcp_auth_returns_auth_url() {
        let output = McpAuthTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "McpAuth".into(),
                    arguments: serde_json::json!({ "server_url": "https://example.com" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("auth_url"));
    }

    #[tokio::test]
    async fn mcp_auth_missing_url() {
        let result = McpAuthTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "McpAuth".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn url_encode_encodes_special_chars() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert!(url_encode("http://localhost:8765/callback").contains("%3A"));
    }
}
