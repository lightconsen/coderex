//! Chrome DevTools Protocol (CDP) client.
//!
//! Connects to Chrome's remote debugging endpoint (default port 9222)
//! via HTTP for discovery and WebSocket for commands.

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const DEFAULT_CDP_URL: &str = "http://localhost:9222";
const WS_TIMEOUT: Duration = Duration::from_secs(30);

/// A tab returned by the CDP HTTP endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct CdpTab {
    /// Tab identifier.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Current URL.
    pub url: String,
    /// WebSocket debugger URL for this tab.
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_url: Option<String>,
}

/// A Chrome CDP client.
#[derive(Debug, Clone)]
pub struct ChromeClient {
    http_client: reqwest::Client,
    cdp_url: String,
}

impl ChromeClient {
    /// Create a new client connecting to the default CDP endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            http_client: reqwest::Client::new(),
            cdp_url: DEFAULT_CDP_URL.into(),
        }
    }

    /// Create a client with a custom CDP endpoint.
    #[must_use]
    pub fn with_endpoint(cdp_url: impl Into<String>) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            cdp_url: cdp_url.into(),
        }
    }

    /// Check whether Chrome's remote debugging is reachable.
    pub async fn is_connected(&self) -> bool {
        self.http_client
            .get(format!("{}/json/version", self.cdp_url))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok()
    }

    /// List all open tabs/pages.
    pub async fn list_tabs(&self) -> Result<Vec<CdpTab>, ChromeError> {
        let resp = self
            .http_client
            .get(format!("{}/json/list", self.cdp_url))
            .send()
            .await
            .map_err(|e| ChromeError::Http(e.to_string()))?;

        let tabs: Vec<CdpTab> = resp
            .json()
            .await
            .map_err(|e| ChromeError::Http(format!("failed to parse tab list: {e}")))?;
        Ok(tabs)
    }

    /// Open a new tab and navigate to the given URL.
    ///
    /// Returns the new tab's websocket debugger URL.
    pub async fn navigate(&self, url: &str) -> Result<String, ChromeError> {
        let resp = self
            .http_client
            .put(format!("{}/json/new?{}", self.cdp_url, url))
            .send()
            .await
            .map_err(|e| ChromeError::Http(e.to_string()))?;

        let tab: CdpTab = resp
            .json()
            .await
            .map_err(|e| ChromeError::Http(format!("failed to parse new tab: {e}")))?;

        tab.ws_url.ok_or_else(|| ChromeError::NoWsUrl)
    }

    /// Execute JavaScript in the given tab and return the result.
    pub async fn execute_js(&self, ws_url: &str, expression: &str) -> Result<Value, ChromeError> {
        let params = serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        });
        self.send_cdp_command(ws_url, "Runtime.evaluate", params)
            .await
    }

    /// Capture a screenshot of the given tab as a base64-encoded PNG.
    pub async fn screenshot(&self, ws_url: &str) -> Result<String, ChromeError> {
        let params = serde_json::json!({
            "format": "png",
            "fromSurface": true,
        });
        let result = self
            .send_cdp_command(ws_url, "Page.captureScreenshot", params)
            .await?;

        result
            .get("data")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| ChromeError::NoResult {
                message: "screenshot data not found".into(),
            })
    }

    /// Get the full HTML content of the given tab.
    pub async fn get_content(&self, ws_url: &str) -> Result<String, ChromeError> {
        let result = self
            .execute_js(ws_url, "document.documentElement.outerHTML")
            .await?;

        result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| ChromeError::NoResult {
                message: "document content not found".into(),
            })
    }

    /// Send a CDP command over WebSocket and wait for the response.
    async fn send_cdp_command(
        &self,
        ws_url: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ChromeError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| ChromeError::WebSocket(e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        let request = serde_json::json!({
            "id": 1,
            "method": method,
            "params": params,
        });

        let msg = WsMessage::Text(request.to_string().into());
        futures_util::SinkExt::send(&mut write, msg).await.map_err(
            |e: tokio_tungstenite::tungstenite::Error| ChromeError::WebSocket(e.to_string()),
        )?;

        let response = tokio::time::timeout(WS_TIMEOUT, async {
            while let Some(Ok(msg)) = futures_util::StreamExt::next(&mut read).await {
                if let WsMessage::Text(text) = msg {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value.get("id").is_some() {
                            return Some(value);
                        }
                    }
                }
            }
            None
        })
        .await
        .map_err(|_| ChromeError::Timeout)?
        .ok_or_else(|| ChromeError::NoResult {
            message: "no response from Chrome".into(),
        })?;

        // Gracefully close the WebSocket
        let _ = futures_util::SinkExt::close(&mut write).await;

        if let Some(error) = response.get("error") {
            return Err(ChromeError::Cdp {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-1),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown CDP error")
                    .into(),
            });
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

impl Default for ChromeClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from Chrome CDP operations.
#[derive(Debug, thiserror::Error)]
pub enum ChromeError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(String),
    /// WebSocket communication failed.
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    /// CDP command returned an error.
    #[error("CDP error {code}: {message}")]
    Cdp {
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },
    /// No WebSocket debugger URL available.
    #[error("no WebSocket debugger URL")]
    NoWsUrl,
    /// Expected result not found in response.
    #[error("missing result: {message}")]
    NoResult {
        /// Description of what was missing.
        message: String,
    },
    /// Operation timed out.
    #[error("operation timed out")]
    Timeout,
}
