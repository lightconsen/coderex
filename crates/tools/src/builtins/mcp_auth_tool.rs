//! McpAuth tool — the interactive half of MCP OAuth.
//!
//! This is the only thing in CodeRex that can obtain a *first* token: it opens
//! the authorization URL, listens for the callback on localhost, and exchanges
//! the code with PKCE (RFC 7636). Everything after that — expiry, refresh,
//! persistence — belongs to `coderex_mcp::oauth`.
//!
//! Two things it needs from the user and cannot supply itself: a browser to
//! visit the URL, and a click to approve. When the browser cannot be opened,
//! the tool returns the URL, the PKCE verifier and the `state` it generated so
//! the flow can be finished by hand — it does not sit waiting for a callback
//! that can never arrive. The hand-off is persisted under
//! `~/.coderex/mcp_oauth_pending.json`, because the finishing call is usually a
//! different invocation: it accepts either a pasted full redirect URL (whose
//! `code` and `state` are parsed and the state checked against the pending
//! flow) or a bare `code` with the `code_verifier` from the first call.
//!
//! Endpoints come from the server's RFC 8414 metadata when it publishes any
//! (`/.well-known/oauth-authorization-server`, then OpenID Connect discovery);
//! otherwise the conventional `/oauth/authorize` and `/oauth/token` paths are
//! used.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Local redirect URI used to capture the OAuth callback.
const LOCAL_REDIRECT_URI: &str = "http://localhost:8765/callback";

/// Pending hand-off flows, beside the token store.
const PENDING_FILE: &str = "mcp_oauth_pending.json";

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
                    "redirect_url": { "type": "string", "description": "The full URL the browser was redirected to after approval. Its code and state are parsed and the state is verified against the pending flow." },
                    "code": { "type": "string", "description": "Authorization code, when finishing by hand after the browser callback did not arrive" },
                    "code_verifier": { "type": "string", "description": "PKCE verifier returned by the earlier call that produced the auth_url (optional when a pending flow is on disk)" }
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let pending_path = pending_path(token_path);
    let pending = load_pending(&pending_path, &server_url);
    let discovery = discover_endpoints(&client, &server_url).await;

    let authorization_endpoint = [
        discovery.authorization_endpoint.clone(),
        pending
            .as_ref()
            .and_then(|pending| pending.authorization_endpoint.clone()),
    ]
    .into_iter()
    .flatten()
    .find(|endpoint| url::Url::parse(endpoint).is_ok())
    .unwrap_or_else(|| format!("{}/oauth/authorize", server_url.trim_end_matches('/')));
    // The token endpoint the exchange is posted to. A pending flow keeps the
    // one discovered during the first call, in case the metadata endpoint is
    // unreachable now but was reachable then.
    let token_endpoint = discovery
        .token_endpoint
        .clone()
        .or_else(|| pending.as_ref().and_then(|p| p.token_endpoint.clone()))
        .unwrap_or_else(|| format!("{}/oauth/token", server_url.trim_end_matches('/')));

    // PKCE: the verifier stays here, the challenge travels in the URL. A
    // code intercepted from the redirect is useless without the verifier.
    let fresh_verifier = code_verifier();
    let challenge = code_challenge(&fresh_verifier);
    let state = format!("coderex-{}", uuid::Uuid::new_v4());

    let redirect_url = extract_string(&input.arguments, "redirect_url").ok();
    let supplied_code = extract_string(&input.arguments, "code").ok();
    let supplied_verifier = extract_string(&input.arguments, "code_verifier").ok();

    let mut auth_url = None;
    let (code, verifier) = if let Some(redirect_url) = redirect_url {
        // Finishing from a pasted redirect URL. The state is only meaningful
        // against the flow that produced it, so there must be a pending flow
        // and the two must agree.
        let (code, received_state) =
            parse_redirect_url(&redirect_url).map_err(|message| ToolError::InvalidInput {
                message: format!("redirect_url is not usable: {message}"),
            })?;
        let Some(pending) = pending else {
            return Err(ToolError::InvalidInput {
                message: format!(
                    "no pending authorization for {server_url}. Run McpAuth without a \
                     redirect_url first, so there is a state and PKCE verifier to check this \
                     redirect against."
                ),
            });
        };
        if received_state.as_deref() != Some(pending.state.as_str()) {
            return Err(ToolError::InvalidInput {
                message: format!(
                    "the redirect URL's state does not match the pending authorization for \
                     {server_url}. A state mismatch means the redirect did not come from this \
                     flow; start again with a fresh McpAuth call."
                ),
            });
        }
        let verifier = supplied_verifier.unwrap_or(pending.code_verifier);
        (code, verifier)
    } else if let Some(code) = supplied_code {
        // A bare code carries no state to check. The verifier must still be
        // the one that built the auth_url this code came from; the pending
        // flow has it when the caller does not.
        let verifier = match supplied_verifier {
            Some(verifier) => verifier,
            None => pending
                .as_ref()
                .map(|pending| pending.code_verifier.clone())
                .ok_or_else(|| ToolError::InvalidInput {
                    message: "a bare `code` needs the `code_verifier` from the call that \
                              produced the auth_url (none is pending for this server)"
                        .into(),
                })?,
        };
        (code, verifier)
    } else {
        // Nobody supplied a finished flow, so this is the browser path. When
        // no browser can be opened there is no callback to wait for: return
        // the hand-off immediately instead of blocking for the timeout.
        let url = authorization_url(
            &authorization_endpoint,
            &client_id,
            &scope,
            &state,
            &challenge,
        )?;
        auth_url = Some(url.clone());

        if let Err(error) = open_browser(&url) {
            let note = format!("Could not open a browser: {error}.");
            return Ok(pending_output(
                &url,
                &fresh_verifier,
                &state,
                &pending_note(
                    &pending_path,
                    &server_url,
                    &state,
                    &fresh_verifier,
                    &authorization_endpoint,
                    &token_endpoint,
                    &note,
                ),
            ));
        }

        // Spawn local HTTP listener to capture callback
        #[cfg(test)]
        let callback_timeout = Duration::from_secs(2);
        #[cfg(not(test))]
        let callback_timeout = Duration::from_secs(300);
        match wait_for_callback(&state, callback_timeout).await {
            Ok(code) => (code, fresh_verifier),
            Err(error) => {
                let note = format!("Waiting for the OAuth callback failed: {error}.");
                return Ok(pending_output(
                    &url,
                    &fresh_verifier,
                    &state,
                    &pending_note(
                        &pending_path,
                        &server_url,
                        &state,
                        &fresh_verifier,
                        &authorization_endpoint,
                        &token_endpoint,
                        &note,
                    ),
                ));
            }
        }
    };

    // Exchange code for token
    let resp = client
        .post(&token_endpoint)
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

    // Persist token locally, in the store `coderex_mcp::oauth` reads back. The
    // endpoints are persisted too, so a refresh in a later process does not
    // depend on discovery succeeding again.
    let expires_in = token_resp
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);
    let refresh_token = token_resp
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let persisted = persist_token_with_endpoints_at(
        token_path,
        &server_url,
        &client_id,
        access_token,
        expires_in,
        refresh_token.as_deref(),
        discovery.token_endpoint.as_deref(),
        discovery.authorization_endpoint.as_deref(),
    );

    // The flow is finished; a pending hand-off for this server is now stale.
    remove_pending(&pending_path, &server_url);

    // The access token itself is deliberately absent: it is a credential,
    // and this output is part of the transcript. What matters to the caller
    // is whether it reached the store the MCP client reads.
    let mut output = serde_json::json!({
        "authenticated": true,
        "persisted": persisted.is_ok(),
    });
    if let Some(auth_url) = auth_url {
        output["auth_url"] = serde_json::Value::String(auth_url);
    }
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

/// Persist a pending hand-off and describe any failure in the note the caller
/// sees, because a hand-off that was not written is not finishable later.
#[allow(clippy::too_many_arguments)]
fn pending_note(
    path: &Path,
    server_url: &str,
    state: &str,
    code_verifier: &str,
    authorization_endpoint: &str,
    token_endpoint: &str,
    reason: &str,
) -> String {
    match save_pending(
        path,
        PendingFlow {
            server_url: server_url.to_string(),
            state: state.to_string(),
            code_verifier: code_verifier.to_string(),
            authorization_endpoint: Some(authorization_endpoint.to_string()),
            token_endpoint: Some(token_endpoint.to_string()),
        },
    ) {
        Ok(()) => reason.to_string(),
        Err(error) => format!(
            "{reason} The pending hand-off could not be saved ({error}), so a later call \
             will need both `code` and `code_verifier`."
        ),
    }
}

/// The response for a flow that has to be finished by hand.
///
/// `state` is included because a pasted redirect URL is checked against it;
/// the verifier is included because the finishing call may be a different
/// invocation from the one that started the flow.
fn pending_output(auth_url: &str, code_verifier: &str, state: &str, note: &str) -> ToolOutput {
    ToolOutput {
        output: serde_json::json!({
            "authenticated": false,
            "auth_url": auth_url,
            "code_verifier": code_verifier,
            "state": state,
            "note": format!(
                "{note} Open auth_url in a browser, then call McpAuth again with the full \
                 redirect_url, or with `code` and this `code_verifier`."
            ),
        })
        .to_string(),
        is_display: true,
    }
}

/// One hand-off flow waiting to be finished by a later call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingFlow {
    server_url: String,
    state: String,
    code_verifier: String,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PendingStore {
    #[serde(default)]
    flows: Vec<PendingFlow>,
}

/// The pending-flow file that sits beside the token store.
fn pending_path(token_path: &Path) -> PathBuf {
    token_path.with_file_name(PENDING_FILE)
}

/// The pending flow for `server_url`, if one was written.
fn load_pending(path: &Path, server_url: &str) -> Option<PendingFlow> {
    let content = std::fs::read_to_string(path).ok()?;
    let store: PendingStore = serde_json::from_str(&content).ok()?;
    store
        .flows
        .into_iter()
        .find(|flow| flow.server_url == server_url)
}

fn save_pending(path: &Path, flow: PendingFlow) -> Result<(), String> {
    // Pending hand-offs are ephemeral: a corrupt file is replaced rather than
    // blocking sign-in. The token store is the durable one and is never
    // overwritten this way.
    let mut store = std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<PendingStore>(&content).ok())
        .unwrap_or_default();
    store
        .flows
        .retain(|entry| entry.server_url != flow.server_url);
    store.flows.push(flow);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let content = serde_json::to_string_pretty(&store).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(path, content).map_err(|e| format!("write: {e}"))
}

fn remove_pending(path: &Path, server_url: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(mut store) = serde_json::from_str::<PendingStore>(&content) else {
        return;
    };
    store.flows.retain(|flow| flow.server_url != server_url);
    if let Ok(content) = serde_json::to_string_pretty(&store) {
        let _ = std::fs::write(path, content);
    }
}

/// Parse `code` and `state` out of a pasted redirect URL.
///
/// The URL comes from the user's address bar, so it is untrusted input: the
/// query is percent-decoded by `Url`, and an `error` parameter is reported
/// rather than mistaken for a missing code.
fn parse_redirect_url(value: &str) -> Result<(String, Option<String>), String> {
    let url = url::Url::parse(value).map_err(|error| format!("not a URL: {error}"))?;

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        return Err(format!("authorization was not granted: {error}"));
    }
    let code = code.ok_or_else(|| "no `code` parameter".to_string())?;
    Ok((code, state))
}

/// Endpoints advertised by the server's metadata documents.
#[derive(Debug, Default)]
struct DiscoveredEndpoints {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
}

/// Fetch RFC 8414 metadata, falling back to OpenID Connect discovery.
///
/// Discovery is best-effort by design: an MCP server that publishes nothing
/// still works at the conventional paths. Nothing here is fatal.
async fn discover_endpoints(client: &reqwest::Client, server_url: &str) -> DiscoveredEndpoints {
    let Some(origin) = origin_of(server_url) else {
        return DiscoveredEndpoints::default();
    };

    for path in [
        ".well-known/oauth-authorization-server",
        ".well-known/openid-configuration",
    ] {
        let url = format!("{origin}/{path}");
        let Ok(response) = client
            .get(&url)
            .header("accept", "application/json")
            .send()
            .await
        else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.text().await else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        let authorization_endpoint = value
            .get("authorization_endpoint")
            .and_then(|v| v.as_str())
            .map(String::from);
        let token_endpoint = value
            .get("token_endpoint")
            .and_then(|v| v.as_str())
            .map(String::from);
        if authorization_endpoint.is_some() || token_endpoint.is_some() {
            tracing::debug!(server = %server_url, "Discovered MCP OAuth endpoints");
            return DiscoveredEndpoints {
                authorization_endpoint,
                token_endpoint,
            };
        }
    }

    tracing::debug!(
        server = %server_url,
        "No OAuth metadata found; using the conventional endpoints"
    );
    DiscoveredEndpoints::default()
}

fn origin_of(server_url: &str) -> Option<String> {
    let url = url::Url::parse(server_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

/// Build the authorization URL with the discovered endpoint and PKCE.
fn authorization_url(
    endpoint: &str,
    client_id: &str,
    scope: &str,
    state: &str,
    challenge: &str,
) -> Result<String, ToolError> {
    let mut url = url::Url::parse(endpoint).map_err(|error| ToolError::InvalidInput {
        message: format!("authorization endpoint {endpoint} is not a URL: {error}"),
    })?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", LOCAL_REDIRECT_URI)
        .append_pair("scope", scope)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
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
    persist_token_with_endpoints_at(
        path,
        server_url,
        client_id,
        access_token,
        expires_in,
        refresh_token,
        None,
        None,
    )
}

/// [`persist_token_at`], also recording the endpoints discovery found.
///
/// Writing them here is what lets a later process refresh without asking the
/// server for its metadata again. `None` leaves the conventional-path fallback
/// in place.
#[allow(clippy::too_many_arguments)]
pub fn persist_token_with_endpoints_at(
    path: &Path,
    server_url: &str,
    client_id: &str,
    access_token: &str,
    expires_in: i64,
    refresh_token: Option<&str>,
    token_endpoint: Option<&str>,
    authorization_endpoint: Option<&str>,
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
        #[serde(default)]
        authorization_endpoint: Option<String>,
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
        token_endpoint: token_endpoint.map(String::from),
        authorization_endpoint: authorization_endpoint.map(String::from),
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
        let dir = tempfile::tempdir().unwrap();
        let output = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                // Nothing listens here, so discovery fails fast and the
                // conventional endpoint is used.
                arguments: serde_json::json!({ "server_url": "http://127.0.0.1:1" }),
            },
            &dir.path().join("mcp_tokens.json"),
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
    /// The headless path must return immediately with everything a later call
    /// needs, and must persist the pending flow for a different invocation.
    #[tokio::test]
    async fn an_unusable_browser_returns_a_finishable_flow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let started = std::time::Instant::now();
        let output = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({ "server_url": "http://127.0.0.1:1" }),
            },
            &path,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
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
        assert!(
            parsed["state"]
                .as_str()
                .is_some_and(|v| v.starts_with("coderex-")),
            "no state to verify a pasted redirect with: {parsed}"
        );
        assert!(parsed["note"].as_str().unwrap().contains("`code`"));

        // No browser means no callback: the call must not sit on the timeout.
        assert!(
            elapsed < Duration::from_secs(2),
            "the headless path blocked for {elapsed:?}"
        );

        // The hand-off has to be readable by the next process.
        let pending = pending_path(&path);
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pending).unwrap()).unwrap();
        assert_eq!(stored["flows"][0]["state"], parsed["state"]);
        assert_eq!(stored["flows"][0]["code_verifier"], parsed["code_verifier"]);
    }

    /// A pasted full redirect URL is parsed, its state checked against the
    /// pending flow, and its code exchanged with the original verifier.
    #[tokio::test]
    async fn a_pasted_redirect_url_finishes_the_pending_flow() {
        let (endpoint, server) = spawn_token_endpoint(
            r#"{"access_token":"tok","refresh_token":"ref","expires_in":3600}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        // First call: no browser in tests, so this is the headless hand-off.
        let pending_output = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({ "server_url": endpoint }),
            },
            &path,
        )
        .await
        .unwrap();
        let pending: serde_json::Value = serde_json::from_str(&pending_output.output).unwrap();
        let state = pending["state"].as_str().unwrap();
        let verifier = pending["code_verifier"].as_str().unwrap();

        // Second call: paste what the browser landed on.
        let redirect_url = format!("http://localhost:8765/callback?code=the_code&state={state}");
        let output = authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": endpoint,
                    "redirect_url": redirect_url,
                }),
            },
            &path,
        )
        .await
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(parsed["authenticated"], true, "{parsed}");
        assert_eq!(parsed["persisted"], true);

        let request = server.join().unwrap();
        assert!(request.contains("code=the_code"), "{request}");
        assert!(
            request.contains(&format!("code_verifier={verifier}")),
            "the original verifier was not used: {request}"
        );

        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(store["states"][0]["access_token"], "tok");

        // The finished flow is no longer pending.
        assert!(load_pending(&pending_path(&path), &endpoint).is_none());
    }

    /// Discovery's advertised endpoints are the ones actually used: the
    /// authorize URL comes from the metadata, and the exchange goes to the
    /// advertised token endpoint rather than `/oauth/token`.
    #[tokio::test]
    async fn discovered_endpoints_drive_the_authorization_and_token_urls() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = format!(
            r#"{{"authorization_endpoint":"{base}/custom/authorize","token_endpoint":"{base}/custom/token"}}"#
        );
        let handle = std::thread::spawn(move || {
            let mut probes = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("POST ") {
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"{"access_token":"tok","expires_in":3600}"#,
                    );
                    return (probes, request);
                }
                probes.push(request);
                write_response(&mut stream, "200 OK", &metadata);
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        // First call has no code: it returns the pending hand-off, whose
        // authorize URL must come from discovery.
        let pending = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({ "server_url": base }),
            },
            &path,
        )
        .await
        .unwrap();
        let pending: serde_json::Value = serde_json::from_str(&pending.output).unwrap();
        assert!(
            pending["auth_url"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{base}/custom/authorize?")),
            "the conventional authorize path was used: {}",
            pending["auth_url"]
        );

        // Second call finishes the flow with a code.
        let output = authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": base,
                    "code": "the_code",
                    "code_verifier": pending["code_verifier"],
                }),
            },
            &path,
        )
        .await
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        assert_eq!(parsed["authenticated"], true, "{parsed}");

        let (probes, exchange) = handle.join().unwrap();
        assert!(!probes.is_empty());
        assert!(
            probes
                .iter()
                .all(|probe| probe.starts_with("GET /.well-known/oauth-authorization-server ")),
            "{probes:?}"
        );
        assert!(
            exchange.starts_with("POST /custom/token "),
            "the conventional token path was used: {exchange}"
        );

        // The discovered endpoints are persisted for the refresh path.
        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            store["states"][0]["token_endpoint"],
            format!("{base}/custom/token")
        );
    }

    /// A redirect from a different flow must be refused rather than exchanged.
    #[tokio::test]
    async fn a_redirect_url_with_the_wrong_state_is_refused() {
        let endpoint = "http://127.0.0.1:1";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({ "server_url": endpoint }),
            },
            &path,
        )
        .await
        .unwrap();

        let error = authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": endpoint,
                    "redirect_url": "http://localhost:8765/callback?code=abc&state=coderex-wrong",
                }),
            },
            &path,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("state does not match"),
            "unexpected error: {error}"
        );
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

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
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
        String::from_utf8_lossy(&request).into_owned()
    }

    fn write_response(stream: &mut std::net::TcpStream, status: &str, body: &str) {
        use std::io::Write;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    /// A token endpoint that also answers (with 404) the discovery probes a
    /// real server URL receives, and records the token request it was sent.
    fn spawn_token_endpoint(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("POST ") {
                    write_response(&mut stream, "200 OK", body);
                    return request;
                }
                // A `.well-known` probe: this server publishes no metadata.
                write_response(&mut stream, "404 Not Found", "");
            }
        });
        (format!("http://{address}"), handle)
    }
}
