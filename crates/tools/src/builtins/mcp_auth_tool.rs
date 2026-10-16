//! McpAuth tool — the interactive half of MCP OAuth.
//!
//! This is the only thing in CodeRex that can obtain a *first* token: it opens
//! the authorization URL, listens for the callback on localhost, and exchanges
//! the code with PKCE (RFC 7636). Everything after that — expiry, refresh,
//! persistence — belongs to `coderex_mcp::oauth`.
//!
//! Two things it needs from the user and cannot supply itself: a browser to
//! visit the URL, and a click to approve. When the browser cannot be opened,
//! the tool returns the URL and the PKCE verifier so the flow can be finished
//! by hand with a second call carrying `code` and `code_verifier` — it does not
//! sit waiting for a callback that can never arrive.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
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
                    "scope": { "type": "string", "description": "OAuth scope (default: mcp)" },
                    "code": { "type": "string", "description": "Authorization code, when finishing by hand after the browser callback did not arrive" },
                    "code_verifier": { "type": "string", "description": "PKCE verifier returned by the earlier call that produced the auth_url" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "authenticated": { "type": "boolean" },
                    "auth_url": { "type": "string" },
                    "code_verifier": { "type": "string" },
                    "persisted": { "type": "boolean" },
                    "note": { "type": "string" }
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
        authenticate(input, &default_token_path()).await
    }
}

/// Run the flow, writing any token it obtains to `token_path`.
///
/// Split out from [`Tool::execute`] so a test can aim the write at a temporary
/// file instead of the user's real `~/.coderex/mcp_tokens.json`.
async fn authenticate(input: ToolInput, token_path: &Path) -> Result<ToolOutput, ToolError> {
    let server_url = extract_string(&input.arguments, "server_url")?;
    let client_id =
        extract_string(&input.arguments, "client_id").unwrap_or_else(|_| "coderex-cli".into());
    let scope = extract_string(&input.arguments, "scope").unwrap_or_else(|_| "mcp".into());

    // PKCE: the verifier stays here, the challenge travels in the URL. A
    // code intercepted from the redirect is useless without the verifier.
    let verifier = code_verifier();
    let challenge = code_challenge(&verifier);

    // Generate a random state parameter
    let state = format!("coderex-{}", uuid::Uuid::new_v4());

    // Build authorization URL
    let auth_url = format!(
        "{server_url}/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        url_encode(LOCAL_REDIRECT_URI),
        url_encode(&scope),
        url_encode(&state),
        url_encode(&challenge),
    );

    // A code supplied by the caller means the flow is being finished by hand:
    // the verifier must be the one that built the auth_url this code came from.
    let (code, verifier) = if let Ok(code) = extract_string(&input.arguments, "code") {
        let verifier = extract_string(&input.arguments, "code_verifier").unwrap_or(verifier);
        (code, verifier)
    } else {
        if let Err(error) = open_browser(&auth_url) {
            return Ok(pending_output(
                &auth_url,
                &verifier,
                &format!("Could not open a browser: {error}."),
            ));
        }

        // Spawn local HTTP listener to capture callback
        #[cfg(test)]
        let callback_timeout = Duration::from_secs(2);
        #[cfg(not(test))]
        let callback_timeout = Duration::from_secs(300);
        match wait_for_callback(&state, callback_timeout).await {
            Ok(code) => (code, verifier),
            Err(e) => {
                return Ok(pending_output(
                    &auth_url,
                    &verifier,
                    &format!("Waiting for the OAuth callback failed: {e}."),
                ));
            }
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
            ("code_verifier", &verifier),
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

    let token_resp: serde_json::Value = resp.json().await.map_err(|e| ToolError::InvalidInput {
        message: format!("failed to parse token response: {e}"),
    })?;

    let access_token = token_resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidInput {
            message: "token response missing access_token".into(),
        })?;

    // Persist token locally, in the store `coderex_mcp::oauth` reads back.
    let expires_in = token_resp
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);
    let refresh_token = token_resp
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let persisted = persist_token_at(
        token_path,
        &server_url,
        &client_id,
        access_token,
        expires_in,
        refresh_token.as_deref(),
    );

    // The access token itself is deliberately absent: it is a credential,
    // and this output is part of the transcript. What matters to the caller
    // is whether it reached the store the MCP client reads.
    let mut output = serde_json::json!({
        "authenticated": true,
        "auth_url": auth_url,
        "persisted": persisted.is_ok(),
    });
    if let Err(error) = persisted {
        output["note"] = serde_json::Value::String(format!(
            "Signed in, but the token could not be saved ({error}), so it will not be \
             available to MCP servers. The refresh path needs it on disk."
        ));
    }

    Ok(ToolOutput {
        output: output.to_string(),
        is_display: true,
    })
}

/// The response for a flow that has to be finished by hand.
fn pending_output(auth_url: &str, code_verifier: &str, note: &str) -> ToolOutput {
    ToolOutput {
        output: serde_json::json!({
            "authenticated": false,
            "auth_url": auth_url,
            "code_verifier": code_verifier,
            "note": format!(
                "{note} Open auth_url in a browser, then call McpAuth again with `code` and \
                 this `code_verifier`."
            ),
        })
        .to_string(),
        is_display: true,
    }
}

/// A PKCE code verifier (RFC 7636 §4.1): 64 unreserved characters.
fn code_verifier() -> String {
    // Two v4 UUIDs give 256 bits of randomness without another dependency.
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The S256 code challenge for `verifier` (RFC 7636 §4.2).
fn code_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
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
    // A test must not launch the user's browser. Returning an error also keeps
    // the test on the manual-completion path, which is the deterministic one.
    #[cfg(test)]
    {
        let _ = url;
        return Err(std::io::Error::other(
            "browser launch is disabled under test",
        ));
    }

    #[cfg(all(not(test), target_os = "macos"))]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(not(test), target_os = "linux"))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(all(not(test), target_os = "windows"))]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()?;
    }
    #[allow(unreachable_code)]
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

/// The token file the MCP client reads: `~/.coderex/mcp_tokens.json`.
#[must_use]
pub fn default_token_path() -> PathBuf {
    home::home_dir()
        .unwrap_or_default()
        .join(".coderex")
        .join("mcp_tokens.json")
}

/// Persist an MCP token into the store that `coderex_mcp::oauth::OAuthManager`
/// reads.
///
/// The on-disk shape is `{"states": [...]}`, one object per server carrying the
/// fields of `McpOAuthState`. `coderex-tools` cannot depend on `coderex-mcp` —
/// the dependency runs the other way — so the shape is mirrored here instead of
/// shared. `crates/mcp/src/oauth.rs` is the source of truth: a field added or
/// renamed there has to change here too. `crates/mcp/tests/mcp_integration.rs`
/// writes a token through this function and reads it back with `OAuthManager`,
/// so drift between the two shows up as a failing test rather than as a session
/// that silently has no credential.
///
/// A store that exists but does not parse is an error, not something to
/// overwrite: quietly replacing it would discard the other servers' tokens.
pub fn persist_token_at(
    path: &Path,
    server_url: &str,
    client_id: &str,
    access_token: &str,
    expires_in: i64,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct TokenStore {
        #[serde(default)]
        states: Vec<TokenEntry>,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct TokenEntry {
        server_url: String,
        client_id: String,
        #[serde(default)]
        client_secret: Option<String>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_at: Option<i64>,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default)]
        token_endpoint: Option<String>,
    }

    let mut store: TokenStore = if path.exists() {
        let content = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("{} is not a readable token store: {e}", path.display()))?
    } else {
        TokenStore::default()
    };

    // Replace this server's entry, keep everyone else's.
    store.states.retain(|e| e.server_url != server_url);
    store.states.push(TokenEntry {
        server_url: server_url.to_string(),
        client_id: client_id.to_string(),
        client_secret: None,
        access_token: Some(access_token.to_string()),
        refresh_token: refresh_token.map(String::from),
        expires_at: Some(chrono::Utc::now().timestamp() + expires_in),
        scopes: Vec::new(),
        token_endpoint: None,
    });

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let content = serde_json::to_string_pretty(&store).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(path, content).map_err(|e| format!("write: {e}"))?;
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

    #[test]
    fn code_challenge_matches_the_rfc_7636_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn code_verifier_is_unreserved_and_long_enough() {
        let verifier = code_verifier();
        assert!(
            (43..=128).contains(&verifier.len()),
            "verifier length {} is outside the RFC 7636 range",
            verifier.len()
        );
        assert!(
            verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')),
            "verifier has characters RFC 7636 does not allow: {verifier}"
        );
        assert_ne!(verifier, code_verifier(), "verifiers must not repeat");
    }

    /// The pending (manual) response has to carry everything a second call
    /// needs, or the instruction it prints is not followable.
    #[tokio::test]
    async fn an_unusable_browser_returns_a_finishable_flow() {
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
        let parsed: serde_json::Value = serde_json::from_str(&output.output).unwrap();

        assert_eq!(parsed["authenticated"], false);
        assert!(
            parsed["auth_url"]
                .as_str()
                .unwrap()
                .contains("code_challenge_method=S256"),
            "the authorize URL does not ask for PKCE: {}",
            parsed["auth_url"]
        );
        assert!(
            parsed["code_verifier"]
                .as_str()
                .is_some_and(|v| v.len() >= 43),
            "no verifier to finish the flow with: {parsed}"
        );
        assert!(parsed["note"].as_str().unwrap().contains("`code`"));
    }

    /// A pasted code is exchanged instead of waiting for a callback that will
    /// never come.
    #[tokio::test]
    async fn a_supplied_code_is_exchanged_with_the_verifier() {
        let (endpoint, server) = spawn_token_endpoint(
            r#"{"access_token":"tok","refresh_token":"ref","expires_in":3600}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        let output = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": endpoint,
                    "code": "the_code",
                    "code_verifier": "the_verifier",
                }),
            },
            &path,
        )
        .await
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(parsed["authenticated"], true);
        assert_eq!(parsed["persisted"], true);
        // The credential itself must not be in the transcript.
        assert!(parsed.get("access_token").is_none(), "{parsed}");

        let request = server.join().unwrap();
        assert!(request.contains("code=the_code"), "{request}");
        assert!(request.contains("code_verifier=the_verifier"), "{request}");
        assert!(
            request.contains("grant_type=authorization_code"),
            "{request}"
        );

        // The exchange is worthless if the token does not reach the shared
        // store, so the write is part of what is being checked.
        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(store["states"][0]["access_token"], "tok");
        assert_eq!(store["states"][0]["refresh_token"], "ref");
    }

    #[tokio::test]
    async fn a_failed_save_is_reported_not_swallowed() {
        let (endpoint, _server) = spawn_token_endpoint(
            r#"{"access_token":"tok","refresh_token":"ref","expires_in":3600}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        std::fs::write(&path, "corrupt").unwrap();

        let output = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": endpoint,
                    "code": "the_code",
                    "code_verifier": "the_verifier",
                }),
            },
            &path,
        )
        .await
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(parsed["persisted"], false);
        assert!(
            parsed["note"]
                .as_str()
                .unwrap()
                .contains("could not be saved"),
            "{parsed}"
        );
    }

    #[test]
    fn persisted_tokens_use_the_shape_the_mcp_manager_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        persist_token_at(
            &path,
            "http://mcp.example",
            "client1",
            "tok",
            3600,
            Some("ref"),
        )
        .unwrap();
        // Second server: the first entry has to survive.
        persist_token_at(&path, "http://other.example", "client1", "tok2", 60, None).unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let states = raw["states"].as_array().unwrap();
        assert_eq!(states.len(), 2);
        let entry = states
            .iter()
            .find(|s| s["server_url"] == "http://mcp.example")
            .unwrap();
        assert_eq!(entry["client_id"], "client1");
        assert_eq!(entry["access_token"], "tok");
        assert_eq!(entry["refresh_token"], "ref");
        assert!(entry["expires_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
        assert!(entry["client_secret"].is_null());
    }

    #[test]
    fn an_unreadable_store_is_reported_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        std::fs::write(&path, "not json at all").unwrap();

        let result = persist_token_at(&path, "http://mcp.example", "client1", "tok", 3600, None);
        assert!(result.is_err(), "a corrupt store was silently replaced");
        // And the corrupt file is still there for the user to look at.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json at all");
    }

    /// A one-shot token endpoint that records the form body it was sent.
    fn spawn_token_endpoint(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}"), handle)
    }
}
