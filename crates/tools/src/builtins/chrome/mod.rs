//! Chrome DevTools Protocol tools — browser automation via CDP.
//!
//! These tools connect to Chrome's remote debugging endpoint (port 9222)
//! and provide navigation, JavaScript execution, screenshot capture, and
//! DOM content extraction.
//!
//! All tools are **feature-gated** behind the `"chrome"` flag.

mod client;

pub use client::ChromeClient;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use serde_json::Value;

/// Key for `ToolContext.extra` to access a shared ChromeClient.
pub const CHROME_CLIENT_KEY: &str = "chrome_client";

fn get_client(ctx: &ToolContext<'_>) -> Option<ChromeClient> {
    ctx.extra
        .get(CHROME_CLIENT_KEY)
        .and_then(|v| v.downcast_ref::<ChromeClient>().cloned())
}

fn extract_string(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

/// List open Chrome tabs.
#[derive(Debug)]
pub struct ChromeListTabsTool;

#[async_trait]
impl Tool for ChromeListTabsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_list_tabs".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tabs": { "type": "array" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        _input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let client = get_client(&ctx).unwrap_or_default();
        if !client.is_connected().await {
            return Err(ToolError::InvalidInput {
                message: "Chrome DevTools not connected. Start Chrome with --remote-debugging-port=9222".into(),
            });
        }
        let tabs = client.list_tabs().await.map_err(|e| ToolError::InvalidInput {
            message: e.to_string(),
        })?;

        let json = serde_json::to_string_pretty(&tabs.iter().map(|t| serde_json::json!({
            "id": t.id,
            "title": t.title,
            "url": t.url,
        })).collect::<Vec<_>>()
        ).unwrap_or_default();

        Ok(ToolOutput {
            output: format!("Open tabs:\n{json}"),
            is_display: true,
        })
    }
}

/// Navigate Chrome to a URL.
#[derive(Debug)]
pub struct ChromeNavigateTool;

#[async_trait]
impl Tool for ChromeNavigateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_navigate".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "URL to navigate to" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" }
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
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let url = extract_string(&input.arguments, "url")?;
        let client = get_client(&ctx).unwrap_or_default();
        if !client.is_connected().await {
            return Err(ToolError::InvalidInput {
                message: "Chrome DevTools not connected. Start Chrome with --remote-debugging-port=9222".into(),
            });
        }

        let ws_url = client.navigate(&url).await.map_err(|e| ToolError::InvalidInput {
            message: e.to_string(),
        })?;

        Ok(ToolOutput {
            output: format!("Navigated to: {url}\nDebugger: {ws_url}"),
            is_display: true,
        })
    }
}

/// Execute JavaScript in Chrome.
#[derive(Debug)]
pub struct ChromeExecuteJsTool;

#[async_trait]
impl Tool for ChromeExecuteJsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_execute_js".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["expression", "ws_url"],
                "properties": {
                    "expression": { "type": "string", "description": "JavaScript expression to evaluate" },
                    "ws_url": { "type": "string", "description": "WebSocket debugger URL of the target tab (from chrome_navigate or chrome_list_tabs)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" }
                }
            }),
            requires_approval: true,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let expression = extract_string(&input.arguments, "expression")?;
        let ws_url = extract_string(&input.arguments, "ws_url")?;
        let client = get_client(&ctx).unwrap_or_default();

        let result = client
            .execute_js(&ws_url, &expression)
            .await
            .map_err(|e| ToolError::InvalidInput {
                message: e.to_string(),
            })?;

        let output = serde_json::to_string_pretty(&result.get("result").unwrap_or(&result)
        ).unwrap_or_else(|_| result.to_string());

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }
}

/// Capture a screenshot of a Chrome tab.
#[derive(Debug)]
pub struct ChromeScreenshotTool;

#[async_trait]
impl Tool for ChromeScreenshotTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_screenshot".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["ws_url"],
                "properties": {
                    "ws_url": { "type": "string", "description": "WebSocket debugger URL of the target tab" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "base64_png": { "type": "string" }
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
        let ws_url = extract_string(&input.arguments, "ws_url")?;
        let client = get_client(&ctx).unwrap_or_default();

        let b64 = client
            .screenshot(&ws_url)
            .await
            .map_err(|e| ToolError::InvalidInput {
                message: e.to_string(),
            })?;

        Ok(ToolOutput {
            output: format!(
                "Screenshot captured (base64 PNG, {} bytes):\n{b64}",
                b64.len()
            ),
            is_display: false,
        })
    }
}

/// Get the HTML content of a Chrome tab.
#[derive(Debug)]
pub struct ChromeGetContentTool;

#[async_trait]
impl Tool for ChromeGetContentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_get_content".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["ws_url"],
                "properties": {
                    "ws_url": { "type": "string", "description": "WebSocket debugger URL of the target tab" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" }
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
        let ws_url = extract_string(&input.arguments, "ws_url")?;
        let client = get_client(&ctx).unwrap_or_default();

        let content = client
            .get_content(&ws_url)
            .await
            .map_err(|e| ToolError::InvalidInput {
                message: e.to_string(),
            })?;

        // Convert HTML to readable text
        let text = crate::builtins::web_fetch_tool::html_to_markdown(&content
        );

        Ok(ToolOutput {
            output: text,
            is_display: true,
        })
    }
}
