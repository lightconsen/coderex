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
//! Endpoints come from RFC 9728 Protected Resource Metadata (which may name an
//! authorization server on a different origin), then that server's RFC 8414 /
//! OpenID Connect metadata; otherwise the conventional `/oauth/authorize` and
//! `/oauth/token` paths are used. When no client id is configured and the
//! authorization server publishes a `registration_endpoint`, a client is
//! registered dynamically (RFC 7591) and the returned credentials are
//! persisted for the refresh path.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use coderex_config::secret_store::TokenPersistence;
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
                    "code_verifier": { "type": "string", "description": "PKCE verifier returned by the earlier call that produced the auth_url (optional when a pending flow is on disk)" },
                    "headless": { "type": "boolean", "description": "Return the authorization URL immediately instead of opening a browser. Used by a headless session's re-authorization handler, which prints the URL for a person to open later." }
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
        // The real sign-in path prefers the OS keychain and falls back to a
        // private file; the store is shared with `coderex_mcp::oauth`.
        let persistence = TokenPersistence::new(default_token_path());
        authenticate_with(input, &persistence).await
    }
}

/// Run the flow, writing any token it obtains to `token_path`.
///
/// Split out from [`Tool::execute`] so a test can aim the write at a temporary
/// file instead of the user's real `~/.coderex/mcp_tokens.json`. An explicit
/// path is file-only and never consults the developer's keychain.
#[cfg(test)]
async fn authenticate(input: ToolInput, token_path: &Path) -> Result<ToolOutput, ToolError> {
    authenticate_with(input, &TokenPersistence::file_only(token_path)).await
}

/// Run the flow against an injected persistence layer.
async fn authenticate_with(
    input: ToolInput,
    persistence: &TokenPersistence,
) -> Result<ToolOutput, ToolError> {
    let token_path = persistence.path();
    let server_url = extract_string(&input.arguments, "server_url")?;
    // The caller naming a client id is what suppresses dynamic registration.
    let requested_client_id = extract_string(&input.arguments, "client_id").ok();
    let scope = extract_string(&input.arguments, "scope").unwrap_or_else(|_| "mcp".into());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let pending_path = pending_path(token_path);
    let pending = load_pending(&pending_path, &server_url);
    let discovery = discover_endpoints(
        &client,
        &server_url,
        requested_client_id.as_deref().unwrap_or(""),
        persistence,
    )
    .await;

    // The authorization server may reject S256. Discovering that here is
    // better than after the user has approved a flow that cannot complete.
    discovery.require_s256(&server_url)?;

    // A client id the caller named wins; otherwise a stored (possibly
    // dynamically registered) one is reused. Registration happens only when
    // neither exists.
    let stored_entry = load_token_entry(persistence, &server_url);
    let mut client_id = requested_client_id
        .clone()
        .or_else(|| stored_entry.as_ref().map(|entry| entry.client_id.clone()))
        .filter(|id| !id.trim().is_empty());
    let mut client_secret = stored_entry
        .as_ref()
        .and_then(|entry| entry.client_secret.clone());

    if requested_client_id.is_none() && client_id.is_none() {
        if let Some(registration_endpoint) = discovery.registration_endpoint.as_deref() {
            let registration = register_client(
                &client,
                registration_endpoint,
                &discovery,
                persistence,
                &server_url,
            )
            .await?;
            client_id = Some(registration.client_id);
            client_secret = registration.client_secret;
        }
    }
    let client_id = client_id.unwrap_or_else(|| "coderex-cli".into());
    let auth_method = select_client_auth_method(
        &discovery.token_endpoint_auth_methods_supported,
        client_secret.is_some(),
    )
    .unwrap_or("none")
    .to_string();
    let resource = discovery.resource(&server_url);

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
            &resource,
        )?;
        auth_url = Some(url.clone());

        // A headless caller (the CLI's re-authorization handler when there is
        // no terminal) cannot wait for a callback and should not launch a
        // browser behind the user's back. Return the URL and hand-off state.
        let headless = input
            .arguments
            .get("headless")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if headless {
            let note = "Headless session: open auth_url in a browser to approve access.";
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
                    note,
                ),
            ));
        }

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

    // Exchange code for token. The client authentication method is the one the
    // authorization server advertised: basic (header), post (form), or none
    // for a public client that relies on PKCE.
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.clone()),
        ("redirect_uri", LOCAL_REDIRECT_URI.to_string()),
        ("code_verifier", verifier.clone()),
        ("resource", resource.clone()),
    ];
    if auth_method != "client_secret_basic" {
        form.push(("client_id", client_id.clone()));
    }
    if auth_method == "client_secret_post" {
        if let Some(secret) = &client_secret {
            form.push(("client_secret", secret.clone()));
        }
    }
    let mut request = client.post(&token_endpoint);
    if auth_method == "client_secret_basic" {
        request = request.basic_auth(&client_id, client_secret.as_deref());
    }
    let resp = request
        .form(&form)
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
    let persisted = persist_authenticated_at(
        persistence,
        &server_url,
        &client_id,
        client_secret.as_deref(),
        access_token,
        expires_in,
        refresh_token.as_deref(),
        &discovery,
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
        images: Vec::new(),
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
        images: Vec::new(),
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

/// One hour is the default when the server sends no usable `max-age`.
const DEFAULT_DISCOVERY_TTL_SECONDS: u64 = 3600;

/// Endpoints advertised by the server's metadata documents.
///
/// Mirrors the fields `coderex_mcp::oauth::OAuthDiscovery` carries. The
/// authorization server metadata may come from a different origin than the
/// MCP endpoint, named by the resource's `authorization_servers`.
#[derive(Debug, Default, Clone)]
struct DiscoveredEndpoints {
    issuer: Option<String>,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    resource: Option<String>,
    scopes_supported: Vec<String>,
    token_endpoint_auth_methods_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
    protected_resource: Option<ProtectedResourceMetadata>,
    resource_metadata_url: Option<String>,
    resource_metadata_etag: Option<String>,
    /// How long the protected resource document may be cached.
    resource_metadata_ttl: u64,
    /// The issuer cache entry, when this result came from a fetch.
    issuer_entry: Option<IssuerMetadataEntry>,
}

impl DiscoveredEndpoints {
    /// The RFC 8707 resource indicator to send.
    fn resource(&self, server_url: &str) -> String {
        self.resource
            .clone()
            .unwrap_or_else(|| server_url.to_string())
    }

    /// Refuse an authorization flow the server's PKCE policy cannot accept.
    fn require_s256(&self, server_url: &str) -> Result<(), ToolError> {
        if self.code_challenge_methods_supported.is_empty()
            || self
                .code_challenge_methods_supported
                .iter()
                .any(|method| method == "S256")
        {
            return Ok(());
        }
        Err(ToolError::InvalidInput {
            message: format!(
                "the authorization server for {server_url} does not support PKCE S256 \
                 (code_challenge_methods_supported = {:?}); refusing to start a flow it \
                 would reject",
                self.code_challenge_methods_supported
            ),
        })
    }

    fn from_entry(entry: &TokenEntry) -> Self {
        Self {
            issuer: entry.issuer.clone(),
            authorization_endpoint: entry.authorization_endpoint.clone(),
            token_endpoint: entry.token_endpoint.clone(),
            registration_endpoint: entry.registration_endpoint.clone(),
            resource: entry.resource.clone(),
            scopes_supported: entry.scopes_supported.clone(),
            token_endpoint_auth_methods_supported: entry
                .token_endpoint_auth_methods_supported
                .clone(),
            grant_types_supported: entry.grant_types_supported.clone(),
            code_challenge_methods_supported: entry.code_challenge_methods_supported.clone(),
            protected_resource: entry.protected_resource.clone(),
            resource_metadata_url: entry.resource_metadata_url.clone(),
            resource_metadata_etag: entry.resource_metadata_etag.clone(),
            resource_metadata_ttl: 0,
            issuer_entry: None,
        }
    }
}

/// RFC 9728 Protected Resource Metadata (mirrored from `coderex_mcp::oauth`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

impl ProtectedResourceMetadata {
    fn issuer(&self) -> Option<String> {
        self.authorization_servers
            .first()
            .and_then(|issuer| normalize_issuer(issuer))
    }
}

/// One cached authorization-server document, keyed by normalized issuer.
///
/// Mirrors `coderex_mcp::oauth::IssuerMetadataCache`; both crates write the
/// same `issuer_metadata` map in the shared token store.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct IssuerMetadataEntry {
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default)]
    grant_types_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
}

impl IssuerMetadataEntry {
    fn is_fresh(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|expires_at| now < expires_at)
    }

    fn has_endpoints(&self) -> bool {
        self.authorization_endpoint.is_some() || self.token_endpoint.is_some()
    }

    fn into_discovery(self) -> DiscoveredEndpoints {
        DiscoveredEndpoints {
            issuer: self.issuer,
            authorization_endpoint: self.authorization_endpoint,
            token_endpoint: self.token_endpoint,
            registration_endpoint: self.registration_endpoint,
            scopes_supported: self.scopes_supported,
            token_endpoint_auth_methods_supported: self.token_endpoint_auth_methods_supported,
            grant_types_supported: self.grant_types_supported,
            code_challenge_methods_supported: self.code_challenge_methods_supported,
            ..DiscoveredEndpoints::default()
        }
    }
}

/// Pick a client authentication method from what the server advertises.
///
/// Mirrors `coderex_mcp::oauth::select_client_auth_method`: basic wins when a
/// secret is available (it keeps the secret out of logs and bodies), `none` is
/// the public-client default, and `None` means no advertised method fits the
/// credentials on hand.
fn select_client_auth_method(advertised: &[String], has_secret: bool) -> Option<&'static str> {
    let offers = |method: &str| advertised.is_empty() || advertised.iter().any(|m| m == method);
    if has_secret {
        if offers("client_secret_basic") {
            Some("client_secret_basic")
        } else if offers("client_secret_post") {
            Some("client_secret_post")
        } else {
            None
        }
    } else if offers("none") {
        Some("none")
    } else {
        None
    }
}

/// The RFC 9728 well-known URL: the path is inserted after the well-known
/// segment, not appended to the resource URL.
fn protected_resource_candidate(resource_url: &str) -> Option<String> {
    let normalized = normalize_issuer(resource_url)?;
    let url = url::Url::parse(&normalized).ok()?;
    if !allows_plain_http(&url) {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    Some(format!(
        "{origin}/.well-known/oauth-protected-resource{path}"
    ))
}

/// Normalize an issuer for discovery and comparison.
///
/// Query and fragment are irrelevant to identity, and a trailing slash is not
/// part of it. The path is kept: `https://id.example/realms/demo` and
/// `https://id.example` are different issuers.
fn normalize_issuer(value: &str) -> Option<String> {
    let mut url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    let normalized = url.as_str().trim_end_matches('/').to_string();
    (!normalized.is_empty()).then_some(normalized)
}

/// Whether discovery may talk to this URL in plain HTTP.
///
/// HTTPS everywhere, except loopback: a local development server has no
/// certificate to present, and the traffic never leaves the machine.
fn allows_plain_http(url: &url::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// The metadata documents to try, in precedence order.
///
/// RFC 8414 inserts `/.well-known/oauth-authorization-server` *before* the
/// issuer's path, while OpenID Connect appends
/// `/.well-known/openid-configuration` *after* it. Origin-level documents are
/// the fallback. Duplicates are removed so an issuer without a path does not
/// probe the same URL twice.
fn discovery_candidates(issuer: &str) -> Option<Vec<String>> {
    let normalized = normalize_issuer(issuer)?;
    let url = url::Url::parse(&normalized).ok()?;
    if !allows_plain_http(&url) {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let candidates = [
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/oauth-authorization-server"),
        format!("{origin}{path}/.well-known/openid-configuration"),
        format!("{origin}/.well-known/openid-configuration"),
    ];

    let mut unique: Vec<String> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    Some(unique)
}

/// How long a metadata document may be reused, per `Cache-Control`.
///
/// `max-age` wins; without it `Expires` (relative to `Date`) is used. A missing
/// header falls back to one hour. `no-store` and `no-cache` mean "fetch again
/// next time", which is a zero-second TTL.
fn cache_max_age(headers: &reqwest::header::HeaderMap) -> u64 {
    let mut max_age = None;
    if let Some(value) = headers
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
    {
        for directive in value.split(',') {
            let directive = directive.trim();
            if directive.eq_ignore_ascii_case("no-store")
                || directive.eq_ignore_ascii_case("no-cache")
            {
                return 0;
            }
            let Some((name, raw)) = directive.split_once('=') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("max-age") {
                if let Ok(seconds) = raw.trim().trim_matches('"').parse::<u64>() {
                    max_age = Some(seconds);
                }
            }
        }
    }
    if let Some(seconds) = max_age {
        return seconds;
    }

    let Some(expires) = headers
        .get(reqwest::header::EXPIRES)
        .and_then(|value| value.to_str().ok())
        .and_then(http_date_epoch)
    else {
        return DEFAULT_DISCOVERY_TTL_SECONDS;
    };
    let date = headers
        .get(reqwest::header::DATE)
        .and_then(|value| value.to_str().ok())
        .and_then(http_date_epoch)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    u64::try_from(expires.saturating_sub(date)).unwrap_or(0)
}

/// Parse an HTTP date (RFC 1123 / RFC 850 / asctime shapes).
fn http_date_epoch(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
        .ok()
        .map(|date| date.timestamp())
}

/// Whether a fetched document describes the issuer we asked about.
///
/// Normalized exact match: a trailing slash or query string does not make a
/// different issuer, but a different host or path does. A document without an
/// `issuer` is accepted for compatibility with servers that publish only
/// endpoint fields.
fn issuer_matches(value: &serde_json::Value, requested: &str) -> bool {
    value
        .get("issuer")
        .and_then(|issuer| issuer.as_str())
        .map_or(true, |advertised| {
            normalize_issuer(advertised).as_deref() == Some(requested)
        })
}

/// Fresh discovery metadata already persisted for `server_url`, if any.
///
/// This is the read half of the cache that both `McpAuth` and
/// `coderex_mcp::oauth::OAuthManager` share through the token store, so a
/// second sign-in attempt does not re-probe metadata that is still valid. An
/// entry with no recorded issuer predates RFC 9728, so it is re-discovered
/// rather than trusted.
fn load_cached_discovery(
    persistence: &TokenPersistence,
    server_url: &str,
) -> Option<DiscoveredEndpoints> {
    let store = load_token_store(persistence).ok()?;
    let entry = store
        .states
        .into_iter()
        .find(|entry| entry.server_url == server_url)?;
    let expires_at = entry.discovery_expires_at?;
    if expires_at <= chrono::Utc::now().timestamp() {
        return None;
    }
    if entry.authorization_endpoint.is_none() && entry.token_endpoint.is_none() {
        return None;
    }
    if entry.issuer.is_none() {
        return None;
    }
    Some(DiscoveredEndpoints::from_entry(&entry))
}

/// Fetch the Protected Resource Metadata document, reusing a cached copy and
/// revalidating it with `If-None-Match` when it has an `ETag`.
async fn fetch_protected_resource(
    client: &reqwest::Client,
    server_url: &str,
    persistence: &TokenPersistence,
) -> Option<(ProtectedResourceMetadata, String, Option<String>, u64)> {
    let entry = load_token_entry(persistence, server_url);
    let now = chrono::Utc::now().timestamp();
    if let Some(entry) = &entry {
        if let (Some(metadata), Some(expires_at)) = (
            &entry.protected_resource,
            entry.resource_metadata_expires_at,
        ) {
            if now < expires_at {
                if let Some(url) = entry.resource_metadata_url.clone() {
                    return Some((
                        metadata.clone(),
                        url,
                        entry.resource_metadata_etag.clone(),
                        0,
                    ));
                }
            }
        }
    }

    let url = protected_resource_candidate(server_url)?;
    let etag = entry.as_ref().and_then(|entry| {
        (entry.resource_metadata_url.as_deref() == Some(url.as_str()))
            .then(|| entry.resource_metadata_etag.clone())
            .flatten()
    });

    let mut request = client.get(&url).header("accept", "application/json");
    if let Some(etag) = etag.as_deref() {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await.ok()?;
    let status = response.status();
    let max_age = cache_max_age(response.headers());
    let response_etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(String::from)
        .or(etag);
    if status == reqwest::StatusCode::NOT_MODIFIED {
        let entry = entry?;
        let metadata = entry.protected_resource?;
        return Some((metadata, url, response_etag, max_age));
    }
    if !status.is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    let metadata: ProtectedResourceMetadata = serde_json::from_str(&body).ok()?;
    if metadata.authorization_servers.is_empty() && metadata.resource.is_none() {
        return None;
    }
    Some((metadata, url, response_etag, max_age))
}

/// Fetch the authorization server's metadata, keyed by its normalized issuer.
///
/// Returns the parsed result and the cache entry to persist. A cached entry is
/// reused when fresh, and revalidated with `If-None-Match` otherwise.
async fn fetch_issuer_metadata(
    client: &reqwest::Client,
    issuer: &str,
    cached: Option<&IssuerMetadataEntry>,
) -> Option<(DiscoveredEndpoints, IssuerMetadataEntry)> {
    let now = chrono::Utc::now().timestamp();
    if let Some(cached) = cached {
        if cached.is_fresh(now) && cached.has_endpoints() {
            return Some((cached.clone().into_discovery(), cached.clone()));
        }
    }
    let candidates = discovery_candidates(issuer)?;
    for candidate in candidates {
        let conditional = cached
            .filter(|entry| entry.source_url.as_deref() == Some(candidate.as_str()))
            .and_then(|entry| entry.etag.as_deref());
        let mut request = client.get(&candidate).header("accept", "application/json");
        if let Some(etag) = conditional {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let Ok(response) = request.send().await else {
            continue;
        };
        let status = response.status();
        let max_age = cache_max_age(response.headers());
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(String::from)
            .or_else(|| conditional.map(String::from));
        if status == reqwest::StatusCode::NOT_MODIFIED {
            let Some(cached) = cached else {
                continue;
            };
            let mut entry = cached.clone();
            entry.expires_at = Some(now.saturating_add(i64::try_from(max_age).unwrap_or(i64::MAX)));
            entry.etag = etag;
            entry.source_url = Some(candidate);
            return Some((entry.clone().into_discovery(), entry));
        }
        if !status.is_success() {
            continue;
        }
        let Ok(body) = response.text().await else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        if !issuer_matches(&value, issuer) {
            tracing::debug!(
                candidate = %candidate,
                "Rejecting OAuth metadata for a different issuer"
            );
            continue;
        }
        let authorization_endpoint = value
            .get("authorization_endpoint")
            .and_then(|v| v.as_str())
            .map(String::from);
        let token_endpoint = value
            .get("token_endpoint")
            .and_then(|v| v.as_str())
            .map(String::from);
        if authorization_endpoint.is_none() && token_endpoint.is_none() {
            continue;
        }
        let strings = |field: &str| -> Vec<String> {
            value
                .get(field)
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let entry = IssuerMetadataEntry {
            issuer: value
                .get("issuer")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| Some(issuer.to_string())),
            authorization_endpoint,
            token_endpoint,
            registration_endpoint: value
                .get("registration_endpoint")
                .and_then(|v| v.as_str())
                .map(String::from),
            scopes_supported: strings("scopes_supported"),
            token_endpoint_auth_methods_supported: strings("token_endpoint_auth_methods_supported"),
            grant_types_supported: strings("grant_types_supported"),
            code_challenge_methods_supported: strings("code_challenge_methods_supported"),
            expires_at: Some(now.saturating_add(i64::try_from(max_age).unwrap_or(i64::MAX))),
            etag,
            source_url: Some(candidate),
        };
        return Some((entry.clone().into_discovery(), entry));
    }
    None
}

/// Fetch RFC 9728 resource metadata, then RFC 8414 / OpenID Connect
/// authorization-server metadata, falling back to the conventional paths when
/// nothing usable is published.
///
/// The issuer is the resource's first `authorization_servers` entry when the
/// resource publishes one, otherwise `server_url` normalized (path kept). A
/// fresh cache is returned without a request.
///
/// Discovery is best-effort by design: an MCP server that publishes nothing
/// still works at the conventional paths. Nothing here is fatal except a
/// server that explicitly refuses S256, which [`DiscoveredEndpoints::require_s256`]
/// reports to the caller.
async fn discover_endpoints(
    client: &reqwest::Client,
    server_url: &str,
    client_id: &str,
    persistence: &TokenPersistence,
) -> DiscoveredEndpoints {
    if let Some(cached) = load_cached_discovery(persistence, server_url) {
        tracing::debug!(server = %server_url, "Using cached MCP OAuth metadata");
        return cached;
    }

    let Some(normalized) = normalize_issuer(server_url) else {
        return DiscoveredEndpoints::default();
    };

    // 1. RFC 9728: the resource may name an authorization server elsewhere.
    let protected = fetch_protected_resource(client, server_url, persistence).await;
    let (protected_metadata, protected_url, protected_etag, protected_ttl) = match protected {
        Some((metadata, url, etag, ttl)) => (Some(metadata), Some(url), etag, ttl),
        None => (None, None, None, 0),
    };
    let issuer = protected_metadata
        .as_ref()
        .and_then(ProtectedResourceMetadata::issuer)
        .unwrap_or_else(|| normalized.clone());
    let resource = protected_metadata
        .as_ref()
        .and_then(|metadata| metadata.resource.clone())
        .unwrap_or_else(|| server_url.to_string());

    // 2. RFC 8414 / OIDC metadata, cached by normalized issuer.
    let cached_entry = load_token_store(persistence)
        .ok()
        .and_then(|store| store.issuer_metadata.get(&issuer).cloned());
    let Some((mut discovery, issuer_entry)) =
        fetch_issuer_metadata(client, &issuer, cached_entry.as_ref()).await
    else {
        tracing::debug!(
            server = %server_url,
            issuer = %issuer,
            "No authorization server metadata; using the conventional endpoints"
        );
        return DiscoveredEndpoints {
            issuer: Some(issuer),
            resource: Some(resource),
            protected_resource: protected_metadata.clone(),
            resource_metadata_url: protected_url,
            resource_metadata_etag: protected_etag,
            resource_metadata_ttl: protected_ttl,
            scopes_supported: protected_metadata
                .as_ref()
                .map(|metadata| metadata.scopes_supported.clone())
                .unwrap_or_default(),
            ..DiscoveredEndpoints::default()
        };
    };

    // The cache key is the normalized issuer, not the raw document URL.
    discovery.issuer = Some(issuer);
    discovery.resource = Some(resource);
    discovery.protected_resource = protected_metadata;
    discovery.resource_metadata_url = protected_url;
    discovery.resource_metadata_etag = protected_etag;
    discovery.resource_metadata_ttl = protected_ttl;
    if discovery.scopes_supported.is_empty() {
        discovery.scopes_supported = discovery
            .protected_resource
            .as_ref()
            .map(|metadata| metadata.scopes_supported.clone())
            .unwrap_or_default();
    }
    discovery.issuer_entry = Some(issuer_entry);

    tracing::debug!(server = %server_url, "Discovered MCP OAuth endpoints");

    // Best-effort: a cache write must never stop a sign-in.
    let _ = cache_discovery_record_at(persistence, server_url, client_id, &discovery);

    discovery
}

/// Build the authorization URL with the discovered endpoint, PKCE and the
/// RFC 8707 resource indicator.
fn authorization_url(
    endpoint: &str,
    client_id: &str,
    scope: &str,
    state: &str,
    challenge: &str,
    resource: &str,
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
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", resource);
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

/// The token store shape shared with `coderex_mcp::oauth`.
///
/// Kept here rather than imported because the dependency runs `tools <- mcp`.
/// Every field has a default so a store written by an older version still
/// parses.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct TokenStore {
    #[serde(default)]
    states: Vec<TokenEntry>,
    /// Authorization-server metadata keyed by normalized issuer, mirroring
    /// `coderex_mcp::oauth`'s `issuer_metadata` map. Two MCP servers behind the
    /// same authorization server share the document.
    #[serde(default)]
    issuer_metadata: std::collections::BTreeMap<String, IssuerMetadataEntry>,
}

/// One server's persisted OAuth state.
///
/// This mirrors `coderex_mcp::oauth::McpOAuthState` field for field except for
/// `Default`-able ordering; `crates/mcp/tests/mcp_integration.rs` reads a store
/// written here through `OAuthManager`, so a field renamed or retyped on one
/// side breaks a test rather than a sign-in.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
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
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    discovery_expires_at: Option<i64>,
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    protected_resource: Option<ProtectedResourceMetadata>,
    #[serde(default)]
    resource_metadata_url: Option<String>,
    #[serde(default)]
    resource_metadata_expires_at: Option<i64>,
    #[serde(default)]
    resource_metadata_etag: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default)]
    grant_types_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

impl TokenEntry {
    fn blank(server_url: &str, client_id: &str) -> Self {
        Self {
            server_url: server_url.to_string(),
            client_id: client_id.to_string(),
            client_secret: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            scopes: Vec::new(),
            token_endpoint: None,
            authorization_endpoint: None,
            scopes_supported: Vec::new(),
            discovery_expires_at: None,
            issuer: None,
            resource: None,
            protected_resource: None,
            resource_metadata_url: None,
            resource_metadata_expires_at: None,
            resource_metadata_etag: None,
            registration_endpoint: None,
            token_endpoint_auth_method: None,
            token_endpoint_auth_methods_supported: Vec::new(),
            grant_types_supported: Vec::new(),
            code_challenge_methods_supported: Vec::new(),
        }
    }
}

fn load_token_store(persistence: &TokenPersistence) -> Result<TokenStore, String> {
    let Some(content) = persistence.load().map_err(|e| format!("read: {e}"))? else {
        return Ok(TokenStore::default());
    };
    serde_json::from_str(&content).map_err(|e| {
        format!(
            "{} is not a readable token store: {e}",
            persistence.path().display()
        )
    })
}

fn save_token_store(persistence: &TokenPersistence, store: &TokenStore) -> Result<(), String> {
    let content = serde_json::to_string_pretty(store).map_err(|e| format!("serialize: {e}"))?;
    persistence
        .save(&content)
        .map_err(|e| format!("write: {e}"))
}

/// The persisted entry for one server, if the store has one.
fn load_token_entry(persistence: &TokenPersistence, server_url: &str) -> Option<TokenEntry> {
    load_token_store(persistence)
        .ok()?
        .states
        .into_iter()
        .find(|entry| entry.server_url == server_url)
}

/// Borrow the entry for `server_url`, adding a blank one when absent.
fn entry_for<'a>(
    store: &'a mut TokenStore,
    server_url: &str,
    client_id: &str,
) -> &'a mut TokenEntry {
    if let Some(index) = store
        .states
        .iter()
        .position(|entry| entry.server_url == server_url)
    {
        &mut store.states[index]
    } else {
        store.states.push(TokenEntry::blank(server_url, client_id));
        store.states.last_mut().expect("an entry was just pushed")
    }
}

/// [`persist_token_at`], also recording the endpoints discovery found.
///
/// Writing them here is what lets a later process refresh without asking the
/// server for its metadata again. `None` leaves whatever was previously
/// recorded — including the conventional-path fallback — in place. Cached
/// `scopes_supported` and the discovery expiry are preserved.
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
    persist_token_with_endpoints(
        &TokenPersistence::file_only(path),
        server_url,
        client_id,
        access_token,
        expires_in,
        refresh_token,
        token_endpoint,
        authorization_endpoint,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_token_with_endpoints(
    persistence: &TokenPersistence,
    server_url: &str,
    client_id: &str,
    access_token: &str,
    expires_in: i64,
    refresh_token: Option<&str>,
    token_endpoint: Option<&str>,
    authorization_endpoint: Option<&str>,
) -> Result<(), String> {
    let mut store = load_token_store(persistence)?;
    let expires_at = chrono::Utc::now().timestamp().saturating_add(expires_in);
    let entry = entry_for(&mut store, server_url, client_id);
    entry.client_id = client_id.to_string();
    entry.access_token = Some(access_token.to_string());
    entry.refresh_token = refresh_token.map(String::from);
    entry.expires_at = Some(expires_at);
    if let Some(endpoint) = token_endpoint {
        entry.token_endpoint = Some(endpoint.to_string());
    }
    if let Some(endpoint) = authorization_endpoint {
        entry.authorization_endpoint = Some(endpoint.to_string());
    }
    save_token_store(persistence, &store)
}

/// Persist a completed sign-in together with everything discovery learned.
///
/// This is the write that lets `coderex_mcp::oauth` reuse the authorization
/// server, endpoints, client credentials and resource indicator without
/// discovering anything again. Fields discovery did not learn are left as they
/// were rather than erased.
#[allow(clippy::too_many_arguments)]
fn persist_authenticated_at(
    persistence: &TokenPersistence,
    server_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    access_token: &str,
    expires_in: i64,
    refresh_token: Option<&str>,
    discovery: &DiscoveredEndpoints,
) -> Result<(), String> {
    let mut store = load_token_store(persistence)?;
    let now = chrono::Utc::now().timestamp();
    let entry = entry_for(&mut store, server_url, client_id);
    entry.client_id = client_id.to_string();
    entry.client_secret = client_secret.map(String::from);
    entry.access_token = Some(access_token.to_string());
    entry.refresh_token = refresh_token.map(String::from);
    entry.expires_at = Some(now.saturating_add(expires_in));
    record_discovery_fields(entry, discovery, now);
    if let Some(issuer) = &discovery.issuer {
        if let Some(cache) = &discovery.issuer_entry {
            store.issuer_metadata.insert(issuer.clone(), cache.clone());
        }
    }
    save_token_store(persistence, &store)
}

/// Copy a discovery result into a persisted entry.
fn record_discovery_fields(entry: &mut TokenEntry, discovery: &DiscoveredEndpoints, now: i64) {
    if let Some(issuer) = &discovery.issuer {
        entry.issuer = Some(issuer.clone());
    }
    if let Some(resource) = &discovery.resource {
        entry.resource = Some(resource.clone());
    }
    if let Some(endpoint) = &discovery.authorization_endpoint {
        entry.authorization_endpoint = Some(endpoint.clone());
    }
    if let Some(endpoint) = &discovery.token_endpoint {
        entry.token_endpoint = Some(endpoint.clone());
    }
    if let Some(endpoint) = &discovery.registration_endpoint {
        entry.registration_endpoint = Some(endpoint.clone());
    }
    if !discovery.scopes_supported.is_empty() {
        entry.scopes_supported = discovery.scopes_supported.clone();
    }
    if !discovery.token_endpoint_auth_methods_supported.is_empty() {
        entry.token_endpoint_auth_methods_supported =
            discovery.token_endpoint_auth_methods_supported.clone();
        entry.token_endpoint_auth_method = Some(
            select_client_auth_method(
                &entry.token_endpoint_auth_methods_supported,
                entry.client_secret.is_some(),
            )
            .unwrap_or("none")
            .to_string(),
        );
    }
    if !discovery.grant_types_supported.is_empty() {
        entry.grant_types_supported = discovery.grant_types_supported.clone();
    }
    if !discovery.code_challenge_methods_supported.is_empty() {
        entry.code_challenge_methods_supported = discovery.code_challenge_methods_supported.clone();
    }
    if let Some(metadata) = &discovery.protected_resource {
        entry.protected_resource = Some(metadata.clone());
    }
    if let Some(url) = &discovery.resource_metadata_url {
        entry.resource_metadata_url = Some(url.clone());
    }
    if let Some(etag) = &discovery.resource_metadata_etag {
        entry.resource_metadata_etag = Some(etag.clone());
    } else if discovery.resource_metadata_url.is_some() {
        entry.resource_metadata_etag = None;
    }
    if discovery.resource_metadata_ttl > 0 {
        entry.resource_metadata_expires_at = Some(
            now.saturating_add(i64::try_from(discovery.resource_metadata_ttl).unwrap_or(i64::MAX)),
        );
    }
    if let Some(cache) = &discovery.issuer_entry {
        if let Some(expires_at) = cache.expires_at {
            entry.discovery_expires_at = Some(expires_at);
        }
    }
}

/// Write the authorization-server cache entry and the per-server snapshot.
fn cache_discovery_record_at(
    persistence: &TokenPersistence,
    server_url: &str,
    client_id: &str,
    discovery: &DiscoveredEndpoints,
) -> Result<(), String> {
    let mut store = load_token_store(persistence)?;
    let now = chrono::Utc::now().timestamp();
    let entry = entry_for(&mut store, server_url, client_id);
    record_discovery_fields(entry, discovery, now);
    if let Some(issuer) = &discovery.issuer {
        if let Some(cache) = &discovery.issuer_entry {
            store.issuer_metadata.insert(issuer.clone(), cache.clone());
        }
    }
    save_token_store(persistence, &store)
}

/// Cache discovered authorization-server metadata in the token store.
///
/// This writes the same fields `coderex_mcp::oauth` reads and records when
/// *it* discovers metadata, plus an expiry derived from `Cache-Control:
/// max-age`. Both halves of MCP OAuth then agree on where to authorize, where
/// to exchange a code or refresh token, and when the document must be fetched
/// again. Existing tokens are preserved; a server not yet registered gets an
/// entry with no credential, which `OAuthManager` accepts.
pub fn cache_discovery_at(
    path: &Path,
    server_url: &str,
    client_id: &str,
    authorization_endpoint: Option<&str>,
    token_endpoint: Option<&str>,
    scopes_supported: &[String],
    expires_in: i64,
) -> Result<(), String> {
    cache_discovery(
        &TokenPersistence::file_only(path),
        server_url,
        client_id,
        authorization_endpoint,
        token_endpoint,
        scopes_supported,
        expires_in,
    )
}

fn cache_discovery(
    persistence: &TokenPersistence,
    server_url: &str,
    client_id: &str,
    authorization_endpoint: Option<&str>,
    token_endpoint: Option<&str>,
    scopes_supported: &[String],
    expires_in: i64,
) -> Result<(), String> {
    let mut store = load_token_store(persistence)?;
    let discovery_expires_at = Some(chrono::Utc::now().timestamp().saturating_add(expires_in));
    let issuer = normalize_issuer(server_url);
    let entry = entry_for(&mut store, server_url, client_id);
    if let Some(issuer) = &issuer {
        entry.issuer = Some(issuer.clone());
    }
    if let Some(endpoint) = authorization_endpoint {
        entry.authorization_endpoint = Some(endpoint.to_string());
    }
    if let Some(endpoint) = token_endpoint {
        entry.token_endpoint = Some(endpoint.to_string());
    }
    entry.scopes_supported = scopes_supported.to_vec();
    entry.discovery_expires_at = discovery_expires_at;
    // The authorization-server cache is keyed by the normalized issuer, the
    // same key `coderex_mcp::oauth` uses, so a later discovery from either
    // crate finds it.
    if let Some(issuer) = issuer {
        let cached = store.issuer_metadata.entry(issuer.clone()).or_default();
        cached.issuer = Some(issuer.clone());
        if let Some(endpoint) = authorization_endpoint {
            cached.authorization_endpoint = Some(endpoint.to_string());
        }
        if let Some(endpoint) = token_endpoint {
            cached.token_endpoint = Some(endpoint.to_string());
        }
        if !scopes_supported.is_empty() {
            cached.scopes_supported = scopes_supported.to_vec();
        }
        cached.expires_at = discovery_expires_at;
        if cached.source_url.is_none() {
            cached.source_url =
                discovery_candidates(&issuer).and_then(|candidates| candidates.into_iter().next());
        }
    }
    save_token_store(persistence, &store)
}

/// A client registered dynamically with the authorization server.
struct ClientRegistration {
    client_id: String,
    client_secret: Option<String>,
}

/// Register this client (RFC 7591) and persist what the server issued.
///
/// The request names the loopback redirect, both grants the sign-in and refresh
/// paths use, and S256. It asks for a public client (`none`) unless the server
/// refuses public clients, in which case it asks for the strongest secret-based
/// method it advertises. The returned credentials are written to the shared
/// store immediately, because the second half of a headless sign-in runs in a
/// different invocation.
async fn register_client(
    client: &reqwest::Client,
    endpoint: &str,
    discovery: &DiscoveredEndpoints,
    persistence: &TokenPersistence,
    server_url: &str,
) -> Result<ClientRegistration, ToolError> {
    let advertised = &discovery.token_endpoint_auth_methods_supported;
    let method = if advertised.is_empty() || advertised.iter().any(|method| method == "none") {
        "none"
    } else if advertised
        .iter()
        .any(|method| method == "client_secret_basic")
    {
        "client_secret_basic"
    } else if advertised
        .iter()
        .any(|method| method == "client_secret_post")
    {
        "client_secret_post"
    } else {
        "none"
    };
    let body = serde_json::json!({
        "client_name": "CodeRex",
        "redirect_uris": [LOCAL_REDIRECT_URI],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": method,
        "code_challenge_method": "S256",
    });
    let response = client
        .post(endpoint)
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| ToolError::InvalidInput {
            message: format!("dynamic client registration at {endpoint} failed: {error}"),
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ToolError::InvalidInput {
            message: format!(
                "dynamic client registration at {endpoint} returned HTTP {status}: {body}"
            ),
        });
    }
    let payload: serde_json::Value =
        serde_json::from_str(&body).map_err(|error| ToolError::InvalidInput {
            message: format!("dynamic client registration returned invalid JSON: {error}"),
        })?;
    let client_id = payload
        .get("client_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("dynamic client registration returned no client_id: {body}"),
        })?
        .to_string();
    let client_secret = payload
        .get("client_secret")
        .and_then(|value| value.as_str())
        .map(String::from);

    let mut store = load_token_store(persistence).map_err(|error| ToolError::InvalidInput {
        message: format!("the registered client could not be stored: {error}"),
    })?;
    let entry = entry_for(&mut store, server_url, &client_id);
    entry.client_id = client_id.clone();
    entry.client_secret.clone_from(&client_secret);
    entry.token_endpoint_auth_method = Some(
        select_client_auth_method(advertised, client_secret.is_some())
            .unwrap_or("none")
            .to_string(),
    );
    save_token_store(persistence, &store).map_err(|error| ToolError::InvalidInput {
        message: format!("the registered client could not be stored: {error}"),
    })?;

    Ok(ClientRegistration {
        client_id,
        client_secret,
    })
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

    /// The helpers here mirror `coderex_mcp::oauth`; this pins the shapes the
    /// two crates have to agree on.
    #[test]
    fn discovery_helpers_mirror_the_mcp_crate() {
        assert_eq!(
            normalize_issuer("https://id.example/realms/demo/?a=1#f").as_deref(),
            Some("https://id.example/realms/demo")
        );
        assert_eq!(
            discovery_candidates("https://id.example/realms/demo").unwrap(),
            vec![
                "https://id.example/.well-known/oauth-authorization-server/realms/demo",
                "https://id.example/.well-known/oauth-authorization-server",
                "https://id.example/realms/demo/.well-known/openid-configuration",
                "https://id.example/.well-known/openid-configuration",
            ]
        );
        assert!(discovery_candidates("http://id.example/realms/demo").is_none());

        let matching = serde_json::json!({ "issuer": "https://id.example/realms/demo/" });
        assert!(issuer_matches(&matching, "https://id.example/realms/demo"));
        let mismatched = serde_json::json!({ "issuer": "https://id.example/other" });
        assert!(!issuer_matches(
            &mismatched,
            "https://id.example/realms/demo"
        ));
    }

    /// A Keycloak-style issuer mounted under a path is discovered via RFC 8414
    /// path insertion, and the result is cached so a second invocation does
    /// not probe the metadata again.
    #[tokio::test]
    async fn a_path_mounted_issuer_is_discovered_and_cached() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let issuer = format!("{base}/realms/demo");
        let metadata = format!(
            r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/protocol/openid-connect/auth","token_endpoint":"{issuer}/protocol/openid-connect/token","scopes_supported":["openid","mcp"]}}"#
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
                        r#"{"access_token":"tok","refresh_token":"ref","expires_in":3600}"#,
                    );
                    return (probes, request);
                }
                probes.push(request);
                write_response(&mut stream, "200 OK", &metadata);
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        let pending = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({ "server_url": issuer }),
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
                .starts_with(&format!("{issuer}/protocol/openid-connect/auth?")),
            "the discovered authorize endpoint was not used: {}",
            pending["auth_url"]
        );

        let output = authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": issuer,
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
        assert_eq!(
            probes.len(),
            2,
            "the fresh metadata cache was ignored: {probes:?}"
        );
        assert!(
            probes[0].starts_with("GET /.well-known/oauth-protected-resource/realms/demo "),
            "the protected-resource probe did not come first: {}",
            probes[0]
        );
        assert!(
            probes[1].starts_with("GET /.well-known/oauth-authorization-server/realms/demo "),
            "path insertion was not tried first: {}",
            probes[1]
        );
        assert!(
            exchange.starts_with("POST /realms/demo/protocol/openid-connect/token "),
            "the discovered token endpoint was not used: {exchange}"
        );

        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            store["states"][0]["token_endpoint"],
            format!("{issuer}/protocol/openid-connect/token")
        );
        assert_eq!(
            store["states"][0]["scopes_supported"],
            serde_json::json!(["openid", "mcp"])
        );
        assert!(
            store["states"][0]["discovery_expires_at"]
                .as_i64()
                .is_some_and(|at| at > chrono::Utc::now().timestamp()),
            "the discovery cache has no future expiry: {store}"
        );
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
        assert!(
            probes
                .iter()
                .any(|probe| probe.starts_with("GET /.well-known/oauth-authorization-server ")),
            "{probes:?}"
        );
        assert!(
            probes
                .iter()
                .all(|probe| probe.starts_with("GET /.well-known/oauth-")),
            "only metadata should be probed: {probes:?}"
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

    /// RFC 9728: the resource metadata can name an authorization server on a
    /// different origin. Discovery follows it, the authorization request and
    /// the code exchange carry the RFC 8707 resource indicator, and the public
    /// client (no secret) uses PKCE alone.
    #[tokio::test]
    async fn protected_resource_on_another_origin_drives_discovery() {
        use std::net::TcpListener;

        let as_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let as_base = format!("http://{}", as_listener.local_addr().unwrap());
        let resource_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let resource_base = format!("http://{}", resource_listener.local_addr().unwrap());
        let resource_url = format!("{resource_base}/mcp");

        let metadata = format!(
            r#"{{"issuer":"{as_base}","authorization_endpoint":"{as_base}/authorize","token_endpoint":"{as_base}/token","token_endpoint_auth_methods_supported":["none"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"]}}"#
        );
        let resource_document = format!(
            r#"{{"resource":"{resource_url}","authorization_servers":["{as_base}"],"scopes_supported":["read"]}}"#
        );

        let as_handle = std::thread::spawn(move || {
            let mut metadata_requests = Vec::new();
            loop {
                let (mut stream, _) = as_listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("POST ") {
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"{"access_token":"tok","refresh_token":"ref","expires_in":3600}"#,
                    );
                    return (metadata_requests, request);
                }
                metadata_requests.push(request);
                write_response(&mut stream, "200 OK", &metadata);
            }
        });
        let resource_handle = std::thread::spawn(move || {
            let (mut stream, _) = resource_listener.accept().unwrap();
            let request = read_request(&mut stream);
            write_response(&mut stream, "200 OK", &resource_document);
            request
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        let pending = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({ "server_url": resource_url }),
            },
            &path,
        )
        .await
        .unwrap();
        let pending: serde_json::Value = serde_json::from_str(&pending.output).unwrap();
        let auth_url = url::Url::parse(pending["auth_url"].as_str().unwrap()).unwrap();
        assert_eq!(
            auth_url.origin().ascii_serialization(),
            as_base,
            "the authorization server from the resource metadata was not used: {}",
            pending["auth_url"]
        );
        let query: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some("coderex-cli")
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("resource").map(String::as_str),
            Some(resource_url.as_str())
        );

        let verifier = pending["code_verifier"].as_str().unwrap().to_string();
        authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": resource_url,
                    "code": "the_code",
                    "code_verifier": verifier,
                }),
            },
            &path,
        )
        .await
        .unwrap();

        let resource_request = resource_handle.join().unwrap();
        assert!(
            resource_request.starts_with("GET /.well-known/oauth-protected-resource/mcp "),
            "the path-inserted protected-resource document was not fetched: {resource_request}"
        );

        let (metadata_requests, exchange) = as_handle.join().unwrap();
        assert_eq!(metadata_requests.len(), 1, "{metadata_requests:?}");
        assert!(
            metadata_requests[0].starts_with("GET /.well-known/oauth-authorization-server "),
            "the authorization server's metadata was not fetched: {}",
            metadata_requests[0]
        );
        assert!(exchange.starts_with("POST /token "), "{exchange}");
        assert_eq!(
            form_param(&exchange, "resource").as_deref(),
            Some(resource_url.as_str())
        );
        assert_eq!(
            form_param(&exchange, "client_id").as_deref(),
            Some("coderex-cli")
        );
        assert_eq!(
            form_param(&exchange, "code_verifier").as_deref(),
            Some(verifier.as_str())
        );
        assert_eq!(form_param(&exchange, "client_secret"), None);
        assert!(!exchange.to_ascii_lowercase().contains("\r\nauthorization:"));

        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let state = &store["states"][0];
        assert_eq!(state["issuer"], as_base);
        assert_eq!(state["resource"], resource_url);
        assert_eq!(state["protected_resource"]["resource"], resource_url);
        assert_eq!(
            state["protected_resource"]["authorization_servers"],
            serde_json::json!([as_base])
        );
        assert_eq!(state["client_id"], "coderex-cli");
        assert!(state["client_secret"].is_null(), "{state}");
        assert!(
            store["issuer_metadata"].get(&as_base).is_some(),
            "the authorization server cache was not keyed by the issuer: {store}"
        );
    }

    /// RFC 7591: without a configured client id, discovery's
    /// `registration_endpoint` is used, the issued credentials are persisted,
    /// and both the code exchange and a later process use them — with
    /// `client_secret_basic` when that is what the server advertises.
    #[tokio::test]
    async fn dynamic_registration_persists_a_client_used_for_basic_auth() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = format!(
            r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","token_endpoint_auth_methods_supported":["client_secret_basic"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"]}}"#
        );
        let handle = std::thread::spawn(move || {
            let mut registrations = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("POST /register ") {
                    registrations.push(request);
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"{"client_id":"dyn-client","client_secret":"dyn-secret"}"#,
                    );
                } else if request.starts_with("POST ") {
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"{"access_token":"tok","expires_in":3600}"#,
                    );
                    return (registrations, request);
                } else {
                    write_response(&mut stream, "200 OK", &metadata);
                }
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

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

        // Registration happens before the hand-off is returned, because the
        // finishing call may run in another process.
        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(store["states"][0]["client_id"], "dyn-client", "{store}");
        assert_eq!(store["states"][0]["client_secret"], "dyn-secret");
        assert_eq!(
            store["states"][0]["token_endpoint_auth_method"],
            "client_secret_basic"
        );

        let verifier = pending["code_verifier"].as_str().unwrap().to_string();
        authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": base,
                    "code": "the_code",
                    "code_verifier": verifier,
                }),
            },
            &path,
        )
        .await
        .unwrap();

        let (registrations, exchange) = handle.join().unwrap();
        assert_eq!(
            registrations.len(),
            1,
            "a stored client was registered again"
        );
        let body = registrations[0]
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["client_name"], "CodeRex");
        assert_eq!(
            body["redirect_uris"],
            serde_json::json!(["http://localhost:8765/callback"])
        );
        assert_eq!(
            body["grant_types"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(body["response_types"], serde_json::json!(["code"]));
        assert_eq!(body["token_endpoint_auth_method"], "client_secret_basic");
        assert_eq!(body["code_challenge_method"], "S256");

        // The secret travels in the header, not the body.
        use base64::Engine;
        let credentials = base64::engine::general_purpose::STANDARD.encode("dyn-client:dyn-secret");
        assert!(
            exchange.to_ascii_lowercase().contains(&format!(
                "\r\nauthorization: basic {}",
                credentials.to_ascii_lowercase()
            )),
            "the registered client was not used for basic auth: {exchange}"
        );
        assert_eq!(form_param(&exchange, "client_id"), None);
        assert_eq!(form_param(&exchange, "client_secret"), None);
    }

    /// `client_secret_post` is used for both the registration request and the
    /// code exchange when that is the advertised method.
    #[tokio::test]
    async fn client_secret_post_is_used_when_advertised() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = format!(
            r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","token_endpoint_auth_methods_supported":["client_secret_post"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"]}}"#
        );
        let handle = std::thread::spawn(move || {
            let mut registrations = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("POST /register ") {
                    registrations.push(request);
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"{"client_id":"post-client","client_secret":"post-secret"}"#,
                    );
                } else if request.starts_with("POST ") {
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"{"access_token":"tok","expires_in":3600}"#,
                    );
                    return (registrations, request);
                } else {
                    write_response(&mut stream, "200 OK", &metadata);
                }
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

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
        let verifier = pending["code_verifier"].as_str().unwrap().to_string();
        authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": base,
                    "code": "the_code",
                    "code_verifier": verifier,
                }),
            },
            &path,
        )
        .await
        .unwrap();

        let (registrations, exchange) = handle.join().unwrap();
        let registration: serde_json::Value = serde_json::from_str(
            registrations[0]
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            registration["token_endpoint_auth_method"],
            "client_secret_post"
        );
        assert_eq!(
            form_param(&exchange, "client_id").as_deref(),
            Some("post-client")
        );
        assert_eq!(
            form_param(&exchange, "client_secret").as_deref(),
            Some("post-secret")
        );
        assert!(!exchange.to_ascii_lowercase().contains("\r\nauthorization:"));
    }

    /// A server that does not offer S256 is refused before an authorization
    /// flow is started, rather than after the user has approved one.
    #[tokio::test]
    async fn a_server_without_s256_is_refused() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = format!(
            r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","code_challenge_methods_supported":["plain"]}}"#
        );
        let handle = std::thread::spawn(move || {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("GET /.well-known/oauth-protected-resource") {
                    write_response(&mut stream, "404 Not Found", "");
                    continue;
                }
                write_response(&mut stream, "200 OK", &metadata);
                return request;
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let error = authenticate(
            ToolInput {
                call_id: "1".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({ "server_url": base }),
            },
            &path,
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("does not support PKCE S256"),
            "the refusal was not clear: {message}"
        );
        handle.join().unwrap();
    }

    /// An expired metadata document is revalidated with `If-None-Match`; a
    /// `304 Not Modified` reuses it and refreshes the cache lifetime instead of
    /// forcing another full fetch.
    #[tokio::test]
    async fn expired_metadata_is_revalidated_with_if_none_match() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = format!(
            r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","token_endpoint_auth_methods_supported":["none"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"]}}"#
        );
        let handle = std::thread::spawn(move || {
            let mut metadata_requests = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("POST ") {
                    write_response(
                        &mut stream,
                        "200 OK",
                        r#"{"access_token":"tok","expires_in":3600}"#,
                    );
                    return (metadata_requests, request);
                }
                if request.starts_with("GET /.well-known/oauth-protected-resource") {
                    write_response(&mut stream, "404 Not Found", "");
                    continue;
                }
                metadata_requests.push(request);
                if metadata_requests.len() == 1 {
                    write_response_with_headers(
                        &mut stream,
                        "200 OK",
                        "ETag: \"v1\"\r\nCache-Control: max-age=0\r\n",
                        &metadata,
                    );
                } else {
                    write_response_with_headers(
                        &mut stream,
                        "304 Not Modified",
                        "ETag: \"v1\"\r\nCache-Control: max-age=3600\r\n",
                        "",
                    );
                }
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

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

        let verifier = pending["code_verifier"].as_str().unwrap().to_string();
        authenticate(
            ToolInput {
                call_id: "2".into(),
                name: "McpAuth".into(),
                arguments: serde_json::json!({
                    "server_url": base,
                    "code": "the_code",
                    "code_verifier": verifier,
                }),
            },
            &path,
        )
        .await
        .unwrap();

        let (metadata_requests, exchange) = handle.join().unwrap();
        assert_eq!(metadata_requests.len(), 2, "{metadata_requests:?}");
        assert!(
            !metadata_requests[0]
                .to_ascii_lowercase()
                .contains("if-none-match"),
            "the first fetch was conditional: {}",
            metadata_requests[0]
        );
        assert!(
            metadata_requests[1]
                .to_ascii_lowercase()
                .contains("if-none-match: \"v1\""),
            "the expired document was not revalidated: {}",
            metadata_requests[1]
        );
        assert!(exchange.starts_with("POST /token "), "{exchange}");

        let store: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            store["states"][0]["discovery_expires_at"]
                .as_i64()
                .is_some_and(|at| at > chrono::Utc::now().timestamp()),
            "the 304 did not refresh the cache lifetime: {store}"
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
        write_response_with_headers(stream, status, "", body);
    }

    fn write_response_with_headers(
        stream: &mut std::net::TcpStream,
        status: &str,
        headers: &str,
        body: &str,
    ) {
        use std::io::Write;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    /// A URL-encoded form field in a captured HTTP request.
    fn form_param(request: &str, key: &str) -> Option<String> {
        let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
        url::form_urlencoded::parse(body.as_bytes())
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
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
