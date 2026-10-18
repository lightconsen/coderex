//! Interactive and headless re-authorization for MCP servers.
//!
//! `coderex-mcp` owns the token lifecycle but cannot sign in by itself: the
//! authorization-code + PKCE flow belongs to the `McpAuth` tool. This module
//! installs the bridge. When a server answers `401 invalid_token` and no
//! refresh token can replace the credential, the OAuth manager calls this
//! handler exactly once.
//!
//! * An interactive session runs the same flow `McpAuth` runs: the browser is
//!   opened and the localhost callback completes the exchange.
//! * A headless session (`--print`, a piped prompt, no terminal) does not open
//!   a browser or wait for a callback. It prints the authorization URL and the
//!   instructions for finishing the flow, then reports that a person has to
//!   complete the sign-in. The pending flow is persisted by the tool, so a
//!   later `McpAuth` call can finish it with the redirect URL.

use coderex_mcp::oauth::ReauthHandler;
use coderex_tools::{ExecutionContext, McpAuthTool, Tool, ToolContext, ToolInput};
use std::path::PathBuf;

/// Runs `McpAuth` on behalf of a rejected MCP HTTP request.
#[derive(Debug)]
pub struct CliReauthHandler {
    /// Whether a person is present to approve the authorization.
    interactive: bool,
    /// Working directory for the tool context. The flow does not read files
    /// there, but the context requires one.
    cwd: PathBuf,
}

impl CliReauthHandler {
    /// Build a handler for a session with (or without) a terminal.
    #[must_use]
    pub fn new(interactive: bool, cwd: impl Into<PathBuf>) -> Self {
        Self {
            interactive,
            cwd: cwd.into(),
        }
    }

    /// Drive the `McpAuth` tool and classify its output.
    async fn run(&self, server_url: &str) -> Result<(), String> {
        let tool = McpAuthTool;
        let input = ToolInput {
            call_id: "mcp-reauthorize".to_string(),
            name: "McpAuth".to_string(),
            arguments: serde_json::json!({
                "server_url": server_url,
                // A headless handler asks for the hand-off immediately: it
                // must not launch a browser or wait out the callback timeout.
                "headless": !self.interactive,
            }),
        };
        let execution_context = if self.interactive {
            ExecutionContext::Normal
        } else {
            ExecutionContext::HeadlessSdk
        };
        let context = ToolContext::new(execution_context, &self.cwd);
        let output = tool
            .execute(input, context)
            .await
            .map_err(|error| error.to_string())?;

        match classify_output(&output.output) {
            Ok(()) => Ok(()),
            Err(message) => {
                if !self.interactive {
                    // Headless runs often route tracing away; put the
                    // actionable hand-off on stderr so the operator sees it.
                    eprintln!("MCP re-authorization required for {server_url}: {message}");
                }
                Err(message)
            }
        }
    }
}

/// Turn the `McpAuth` tool's JSON output into `Ok(())` or a human-readable
/// reason the flow did not complete.
///
/// A successful flow reports `authenticated: true`. Anything else carries the
/// authorization URL and a note; both are included so the caller's error is
/// actionable rather than a bare "401".
fn classify_output(output: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(output).map_err(|error| {
        format!("McpAuth returned output that could not be understood: {error}")
    })?;
    if value
        .get("authenticated")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return Ok(());
    }
    let note = value
        .get("note")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("McpAuth did not complete the authorization flow");
    let message = value
        .get("auth_url")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || note.to_string(),
            |url| {
                format!(
                    "{note} Open this URL to approve access, then finish the flow with \
                     McpAuth: {url}"
                )
            },
        );
    Err(message)
}

#[async_trait::async_trait]
impl ReauthHandler for CliReauthHandler {
    async fn reauthorize(&self, server_url: &str) -> Result<(), String> {
        self.run(server_url).await
    }
}

#[cfg(test)]
mod tests {
    use super::classify_output;

    #[test]
    fn a_completed_flow_is_success() {
        assert!(classify_output(r#"{"authenticated":true,"persisted":true}"#).is_ok());
    }

    #[test]
    fn a_headless_handoff_carries_the_url_and_instructions() {
        let output = r#"{
            "authenticated": false,
            "auth_url": "https://id.example/authorize?state=abc",
            "note": "Headless session: open auth_url in a browser to approve access."
        }"#;
        let error = classify_output(output).unwrap_err();
        assert!(error.contains("https://id.example/authorize"), "{error}");
        assert!(error.contains("Open this URL"), "{error}");
    }

    #[test]
    fn an_authentication_failure_without_a_url_still_reports_the_note() {
        let error = classify_output(r#"{"authenticated":false,"note":"state mismatch"}"#)
            .unwrap_err();
        assert_eq!(error, "state mismatch");
    }

    #[test]
    fn malformed_tool_output_is_an_error_rather_than_a_false_success() {
        assert!(classify_output("not json").is_err());
    }
}
