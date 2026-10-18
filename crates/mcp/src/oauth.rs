//! MCP OAuth token bookkeeping.
//!
//! What this module is: a store for the tokens an MCP server issued, and a
//! refresh exchange that trades a refresh token for a fresh access token.
//!
//! What this module is not: a login. Nothing here starts an authorization
//! flow, so nothing here can obtain a *first* token. The interactive
//! authorization-code + PKCE flow lives in the `McpAuth` tool
//! (`crates/tools/src/builtins/mcp_auth_tool.rs`) and is the only thing that
//! can sign in. This module can keep a session alive once that has happened.
//!
//! A refresh needs the server to have handed out a refresh token in the first
//! place. Servers that do not issue one are not refreshable here, and
//! [`OAuthManager::refresh`] says so rather than pretending.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use thiserror::Error;

/// Something went wrong while refreshing or reading a token.
#[derive(Debug, Error)]
pub enum OAuthError {
    /// No token state is registered for this server URL.
    #[error("no OAuth state is registered for {server_url}")]
    UnknownServer {
        /// The server URL that was looked up.
        server_url: String,
    },
    /// The server never issued a refresh token, so there is nothing to trade.
    #[error("no refresh token for {server_url}; run McpAuth to sign in again")]
    NoRefreshToken {
        /// The server URL that was looked up.
        server_url: String,
    },
    /// The token endpoint could not be reached.
    #[error("token endpoint request failed: {0}")]
    Request(String),
    /// The token endpoint answered with a non-success status.
    #[error("token endpoint returned HTTP {status}: {body}")]
    Rejected {
        /// HTTP status code.
        status: u16,
        /// Response body, for the operator.
        body: String,
    },
    /// The token endpoint answered, but not with a usable access token.
    #[error("token response was not usable: {0}")]
    Response(String),
    /// The token file could not be read or written.
    #[error("token store {path} could not be used: {source}")]
    Store {
        /// The file that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Neither authorization-server metadata document could be used.
    #[error("no OAuth server metadata found for {server_url}: {detail}")]
    Discovery {
        /// The server URL that was looked up.
        server_url: String,
        /// Why every candidate document was rejected.
        detail: String,
    },
}

/// OAuth state for an MCP server connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthState {
    /// The server URL.
    pub server_url: String,
    /// OAuth client ID.
    pub client_id: String,
    /// OAuth client secret (if applicable).
    ///
    /// Public clients — the usual case for MCP servers — have none, so this is
    /// optional both in the type and on disk.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Access token.
    #[serde(default)]
    pub access_token: Option<String>,
    /// Refresh token.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Token expiry timestamp (epoch seconds).
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Scopes granted.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The token endpoint to refresh against.
    ///
    /// Left unset by [`McpOAuthState::new`]; [`McpOAuthState::token_endpoint`]
    /// then derives the conventional `{server_url}/oauth/token`, which is the
    /// endpoint the `McpAuth` tool exchanged its code at. [`OAuthManager::discover`]
    /// fills this in from the server's RFC 8414 metadata when it is available.
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// The authorization endpoint the `McpAuth` tool should send the user to.
    ///
    /// Like [`Self::token_endpoint`], this is empty until discovery finds it;
    /// [`McpOAuthState::authorization_endpoint`] falls back to the conventional
    /// `{server_url}/oauth/authorize` path.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
}

impl McpOAuthState {
    /// Create a new OAuth state.
    #[must_use]
    pub fn new(server_url: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            client_id: client_id.into(),
            client_secret: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            scopes: Vec::new(),
            token_endpoint: None,
            authorization_endpoint: None,
        }
    }

    /// Check if the access token is expired.
    ///
    /// A state with no recorded expiry counts as expired: without one there is
    /// no way to tell a fresh token from a stale one, so it is treated as stale.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let now = chrono::Utc::now().timestamp();
                now >= exp
            }
            None => true,
        }
    }

    /// Check if the token is near expiry (within 5 minutes).
    ///
    /// This is the trigger for [`OAuthManager::refresh_if_needed`]: a token
    /// inside the window is replaced *before* it is handed out, so a caller
    /// never receives one that is about to die mid-request.
    #[must_use]
    pub fn is_near_expiry(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let now = chrono::Utc::now().timestamp();
                now >= exp - 300
            }
            None => true,
        }
    }

    /// Check if authenticated.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.access_token.is_some() && !self.is_expired()
    }

    /// The endpoint a refresh is posted to.
    #[must_use]
    pub fn token_endpoint(&self) -> String {
        self.token_endpoint
            .clone()
            .unwrap_or_else(|| format!("{}/oauth/token", self.server_url.trim_end_matches('/')))
    }

    /// The endpoint a browser should be sent to for authorization.
    #[must_use]
    pub fn authorization_endpoint(&self) -> String {
        self.authorization_endpoint
            .clone()
            .unwrap_or_else(|| format!("{}/oauth/authorize", self.server_url.trim_end_matches('/')))
    }

    /// Update tokens from an OAuth flow.
    pub fn update_tokens(&mut self, access: String, refresh: Option<String>, expires_in: i64) {
        self.access_token = Some(access);
        self.refresh_token = refresh;
        self.expires_at = Some(chrono::Utc::now().timestamp() + expires_in);
    }
}

/// Token store for persistence.
///
/// The on-disk shape is `{"states": [...]}`. `coderex-tools` mirrors this
/// shape when the `McpAuth` tool writes a token (`persist_token_at`); there is
/// no shared type because the dependency runs `tools <- mcp`, so this struct
/// is the source of truth and any field change has to be made in both places.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenStore {
    states: Vec<McpOAuthState>,
}

/// The endpoints an authorization server advertises (RFC 8414).
///
/// Both fields are optional in the document, but a document that advertises
/// neither is not useful and is treated as a failed discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthDiscovery {
    /// Where the user is sent to approve the client.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    /// Where an authorization code or refresh token is exchanged.
    #[serde(default)]
    pub token_endpoint: Option<String>,
}

impl OAuthDiscovery {
    fn is_empty(&self) -> bool {
        self.authorization_endpoint.is_none() && self.token_endpoint.is_none()
    }
}

/// The `scheme://host[:port]` part of a server URL, or `None` for anything
/// that is not a usable http(s) URL.
fn origin_of(server_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(server_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

async fn fetch_discovery(client: &reqwest::Client, url: &str) -> Result<OAuthDiscovery, String> {
    let response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid JSON: {error}"))
}

/// Resolves the access token for one MCP server before each HTTP request.
///
/// It holds the same [`OAuthManager`] the rest of the manager uses, behind the
/// manager's lock, so a refresh started by one request updates the state every
/// other request sees. The lock is held only while the token is resolved.
#[derive(Debug, Clone)]
pub struct OAuthTokenProvider {
    /// Weak so that installing providers does not make the manager's OAuth
    /// state un-uniquely-owned; `McpManager::oauth_mut` still works after
    /// startup. The manager outlives every server it starts.
    oauth: Weak<tokio::sync::Mutex<OAuthManager>>,
    server_url: String,
    client: reqwest::Client,
}

impl OAuthTokenProvider {
    /// Build a provider for `server_url` against a shared manager.
    #[must_use]
    pub fn new(
        oauth: Arc<tokio::sync::Mutex<OAuthManager>>,
        server_url: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            oauth: Arc::downgrade(&oauth),
            server_url: server_url.into(),
            client,
        }
    }
}

#[async_trait::async_trait]
impl crate::http::TokenProvider for OAuthTokenProvider {
    async fn access_token(&self) -> Option<String> {
        let Some(shared) = self.oauth.upgrade() else {
            // The manager is gone; there is no token state left to consult.
            return None;
        };
        let mut oauth = shared.lock().await;
        match oauth
            .refresh_if_needed(&self.server_url, &self.client)
            .await
        {
            Ok(token) => token,
            Err(error) => {
                // Degrade to "no credential" rather than failing the request
                // before it is sent: the server can answer 401 honestly.
                tracing::warn!(
                    server = %self.server_url,
                    "MCP OAuth token could not be resolved, sending the request without one: {error}"
                );
                None
            }
        }
    }
}

/// OAuth flow manager.
#[derive(Debug)]
pub struct OAuthManager {
    states: Vec<McpOAuthState>,
    path: PathBuf,
}

impl Default for OAuthManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthManager {
    /// Create a new OAuth manager backed by the default token file.
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: Vec::new(),
            path: Self::token_path(),
        }
    }

    /// Create a new OAuth manager backed by `path`.
    #[must_use]
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            states: Vec::new(),
            path: path.into(),
        }
    }

    /// Load tokens from the default persistence file.
    pub fn load_tokens() -> Result<Self, std::io::Error> {
        Self::load_tokens_from(&Self::token_path())
    }

    /// Load tokens from `path`.
    ///
    /// A missing file is not an error — it just means nothing has signed in yet.
    pub fn load_tokens_from(path: &Path) -> Result<Self, std::io::Error> {
        if !path.exists() {
            return Ok(Self::with_path(path));
        }
        let content = std::fs::read_to_string(path)?;
        let store: TokenStore = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self {
            states: store.states,
            path: path.to_path_buf(),
        })
    }

    /// Save tokens to the default persistence file.
    pub fn save_tokens(&self) -> Result<(), std::io::Error> {
        self.save_tokens_to(&self.path)
    }

    /// Save tokens to `path`.
    pub fn save_tokens_to(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = TokenStore {
            states: self.states.clone(),
        };
        let content = serde_json::to_string_pretty(&store)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, content)
    }

    /// Default token file path: ~/.coderex/mcp_tokens.json
    #[must_use]
    pub fn token_path() -> std::path::PathBuf {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(|h| {
                std::path::PathBuf::from(h)
                    .join(".coderex")
                    .join("mcp_tokens.json")
            })
            .unwrap_or_else(|_| std::path::PathBuf::from(".coderex/mcp_tokens.json"))
    }

    /// Register a server for OAuth.
    pub fn register(&mut self, state: McpOAuthState) {
        self.states.retain(|s| s.server_url != state.server_url);
        self.states.push(state);
    }

    /// Get OAuth state for a server.
    #[must_use]
    pub fn get(&self, server_url: &str) -> Option<&McpOAuthState> {
        self.states.iter().find(|s| s.server_url == server_url)
    }

    /// Get mutable OAuth state.
    #[must_use]
    pub fn get_mut(&mut self, server_url: &str) -> Option<&mut McpOAuthState> {
        self.states.iter_mut().find(|s| s.server_url == server_url)
    }

    /// Check if a server is authenticated.
    #[must_use]
    pub fn is_authenticated(&self, server_url: &str) -> bool {
        self.get(server_url).is_some_and(|s| s.is_authenticated())
    }

    /// List all registered server URLs.
    #[must_use]
    pub fn server_urls(&self) -> Vec<&str> {
        self.states.iter().map(|s| s.server_url.as_str()).collect()
    }

    /// Fetch the server's OAuth metadata and record the endpoints it advertises.
    ///
    /// RFC 8414 puts the document at `{origin}/.well-known/oauth-authorization-server`.
    /// Some servers only publish OpenID Connect discovery, so
    /// `{origin}/.well-known/openid-configuration` is tried when the first
    /// document is missing or unusable. The origin — not the MCP endpoint path —
    /// is used, because the metadata is about the authorization server.
    ///
    /// Discovered endpoints are written into the state for `server_url` (which
    /// is created with the conventional client id if the server has not been
    /// registered yet) and persisted, so the next process does not have to ask
    /// again. Callers that cannot use `Result` — [`OAuthManager::refresh`] —
    /// treat a failure as "no metadata" and fall back to the conventional paths.
    pub async fn discover(
        &mut self,
        server_url: &str,
        client: &reqwest::Client,
    ) -> Result<OAuthDiscovery, OAuthError> {
        let Some(origin) = origin_of(server_url) else {
            return Err(OAuthError::Discovery {
                server_url: server_url.to_string(),
                detail: "the server URL has no usable http(s) origin".to_string(),
            });
        };

        let mut attempts = Vec::new();
        let mut found = None;
        for path in [
            ".well-known/oauth-authorization-server",
            ".well-known/openid-configuration",
        ] {
            let url = format!("{origin}/{path}");
            match fetch_discovery(client, &url).await {
                Ok(discovery) if !discovery.is_empty() => {
                    found = Some(discovery);
                    break;
                }
                Ok(_) => attempts.push(format!("{url}: advertised no endpoints")),
                Err(error) => attempts.push(format!("{url}: {error}")),
            }
        }

        let Some(discovery) = found else {
            return Err(OAuthError::Discovery {
                server_url: server_url.to_string(),
                detail: attempts.join("; "),
            });
        };

        if self.get(server_url).is_none() {
            self.register(McpOAuthState::new(server_url, "coderex-cli"));
        }
        let state = self
            .get_mut(server_url)
            .expect("the state was just registered");
        if discovery.authorization_endpoint.is_some() {
            state.authorization_endpoint = discovery.authorization_endpoint.clone();
        }
        if discovery.token_endpoint.is_some() {
            state.token_endpoint = discovery.token_endpoint.clone();
        }

        self.save_tokens().map_err(|source| OAuthError::Store {
            path: self.path.display().to_string(),
            source,
        })?;

        Ok(discovery)
    }

    /// Trade the stored refresh token for a fresh access token.
    ///
    /// This is a real `grant_type=refresh_token` exchange against the server's
    /// token endpoint, and the result is written back to the token file so a
    /// rotated refresh token survives the process. It fails, rather than
    /// returning the old token, when the server never issued a refresh token —
    /// there is nothing this can do in that case.
    ///
    /// Servers that rotate refresh tokens are handled: a response that carries
    /// a new `refresh_token` replaces the stored one, and a response that omits
    /// it keeps the old one, which is what RFC 6749 §6 says happens.
    pub async fn refresh(
        &mut self,
        server_url: &str,
        client: &reqwest::Client,
    ) -> Result<String, OAuthError> {
        if self.get(server_url).is_none() {
            return Err(OAuthError::UnknownServer {
                server_url: server_url.to_string(),
            });
        }

        // A state that predates discovery (or was written by an older version)
        // has no endpoint recorded. Ask the server once; if it does not
        // advertise one, `token_endpoint()` keeps the conventional path.
        if self
            .get(server_url)
            .is_some_and(|state| state.token_endpoint.is_none())
        {
            let _ = self.discover(server_url, client).await;
        }

        let Some(state) = self.get(server_url) else {
            return Err(OAuthError::UnknownServer {
                server_url: server_url.to_string(),
            });
        };
        let Some(refresh_token) = state.refresh_token.clone() else {
            return Err(OAuthError::NoRefreshToken {
                server_url: server_url.to_string(),
            });
        };
        let endpoint = state.token_endpoint();
        let client_id = state.client_id.clone();
        let client_secret = state.client_secret.clone();

        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ];
        if let Some(secret) = client_secret {
            form.push(("client_secret", secret));
        }

        let response = client
            .post(&endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| OAuthError::Request(e.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| OAuthError::Request(e.to_string()))?;
        if !status.is_success() {
            return Err(OAuthError::Rejected {
                status: status.as_u16(),
                body,
            });
        }

        let payload: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| OAuthError::Response(format!("{e}: {body}")))?;
        let access_token = payload
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| OAuthError::Response(format!("no access_token in {body}")))?
            .to_string();
        let expires_in = payload
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(3600);
        let rotated = payload
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from);

        let state = self
            .get_mut(server_url)
            .expect("state was found a moment ago and nothing removed it");
        let refresh = rotated.or_else(|| state.refresh_token.clone());
        state.update_tokens(access_token.clone(), refresh, expires_in);

        // A rotated refresh token is worthless to anyone but the next process
        // unless it reaches the file.
        self.save_tokens().map_err(|source| OAuthError::Store {
            path: self.path.display().to_string(),
            source,
        })?;

        Ok(access_token)
    }

    /// The access token to send to `server_url`, refreshing it first if it is
    /// inside the 5-minute expiry window.
    ///
    /// This is the call site for [`McpOAuthState::is_near_expiry`]. Returns
    /// `Ok(None)` when there is no usable token — no state, or an expired one
    /// with no refresh token — so the caller can connect unauthenticated and
    /// let the server reject it, rather than pretending it has a credential.
    ///
    /// A failed refresh falls back to the current token while that token is
    /// still valid, so a token endpoint that is briefly down does not break a
    /// working session. Once the token has actually expired, the error is
    /// returned.
    pub async fn refresh_if_needed(
        &mut self,
        server_url: &str,
        client: &reqwest::Client,
    ) -> Result<Option<String>, OAuthError> {
        let Some(state) = self.get(server_url) else {
            return Ok(None);
        };

        if !state.is_near_expiry() {
            return Ok(state.access_token.clone());
        }

        let still_valid = state.is_authenticated() && state.access_token.is_some();

        if state.refresh_token.is_none() {
            return Ok(still_valid.then(|| {
                state
                    .access_token
                    .clone()
                    .expect("still_valid checked the token was there")
            }));
        }

        match self.refresh(server_url, client).await {
            Ok(token) => Ok(Some(token)),
            Err(error) => {
                if still_valid {
                    Ok(self.get(server_url).and_then(|s| s.access_token.clone()))
                } else {
                    Err(error)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn new_state_is_not_authenticated() {
        let state = McpOAuthState::new("http://localhost", "client1");
        assert!(!state.is_authenticated());
    }

    #[test]
    fn update_tokens_authenticates() {
        let mut state = McpOAuthState::new("http://localhost", "client1");
        state.update_tokens("access_token_123".into(), Some("refresh_123".into()), 3600);
        assert!(state.is_authenticated());
        assert!(state.access_token.is_some());
        assert!(state.refresh_token.is_some());
    }

    #[test]
    fn expired_token_not_authenticated() {
        let mut state = McpOAuthState::new("http://localhost", "client1");
        // Set expiry in the past
        state.access_token = Some("token".into());
        state.expires_at = Some(0);
        assert!(!state.is_authenticated());
        assert!(state.is_expired());
    }

    #[test]
    fn oauth_manager_register() {
        let mut mgr = OAuthManager::new();
        let state = McpOAuthState::new("http://server1", "client1");
        mgr.register(state);
        assert_eq!(mgr.server_urls().len(), 1);
        assert!(!mgr.is_authenticated("http://server1"));
    }

    #[test]
    fn oauth_manager_get_mut() {
        let mut mgr = OAuthManager::new();
        let state = McpOAuthState::new("http://server1", "client1");
        mgr.register(state);
        if let Some(s) = mgr.get_mut("http://server1") {
            s.update_tokens("tok".into(), None, 3600);
        }
        assert!(mgr.is_authenticated("http://server1"));
    }

    #[test]
    fn near_expiry_detects_within_5_minutes() {
        let mut state = McpOAuthState::new("http://localhost", "client1");
        state.update_tokens("tok".into(), None, 300); // expires in 5 min exactly
        assert!(state.is_near_expiry());
        assert!(!state.is_expired());
    }

    #[test]
    fn save_and_load_tokens_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        let mut mgr = OAuthManager::with_path(&path);
        let mut state = McpOAuthState::new("http://server1", "client1");
        state.update_tokens("access".into(), Some("refresh".into()), 3600);
        mgr.register(state);

        mgr.save_tokens().unwrap();
        let loaded = OAuthManager::load_tokens_from(&path).unwrap();
        assert!(loaded.is_authenticated("http://server1"));
        let s = loaded.get("http://server1").unwrap();
        assert_eq!(s.access_token.as_ref().unwrap(), "access");
        assert_eq!(s.refresh_token.as_ref().unwrap(), "refresh");
    }

    #[test]
    fn register_replaces_an_existing_entry_for_the_same_server() {
        let mut mgr = OAuthManager::new();
        mgr.register(McpOAuthState::new("http://server1", "old"));
        mgr.register(McpOAuthState::new("http://server1", "new"));
        assert_eq!(mgr.server_urls().len(), 1);
        assert_eq!(mgr.get("http://server1").unwrap().client_id, "new");
    }

    /// A one-shot token endpoint that records the form body it was sent.
    fn spawn_token_endpoint(
        status: u16,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
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
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{address}"), handle)
    }

    fn state_against(endpoint: &str, refresh: Option<&str>, expires_in: i64) -> McpOAuthState {
        let mut state = McpOAuthState::new("http://mcp.example", "client1");
        state.token_endpoint = Some(endpoint.to_string());
        state.access_token = Some("old_access".into());
        state.refresh_token = refresh.map(String::from);
        state.expires_at = Some(chrono::Utc::now().timestamp() + expires_in);
        state
    }

    #[tokio::test]
    async fn refresh_posts_the_refresh_grant_and_stores_the_new_token() {
        let (endpoint, server) = spawn_token_endpoint(
            200,
            r#"{"access_token":"new_access","refresh_token":"rotated","expires_in":1200}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");

        let mut mgr = OAuthManager::with_path(&path);
        mgr.register(state_against(&endpoint, Some("refresh_me"), 60));

        let token = mgr
            .refresh("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(token, "new_access");

        let request = server.join().unwrap();
        assert!(
            request.contains("grant_type=refresh_token"),
            "not a refresh grant: {request}"
        );
        assert!(
            request.contains("refresh_token=refresh_me"),
            "refresh token not sent: {request}"
        );
        assert!(
            request.contains("client_id=client1"),
            "client id not sent: {request}"
        );

        let state = mgr.get("http://mcp.example").unwrap();
        assert_eq!(state.access_token.as_deref(), Some("new_access"));
        assert_eq!(state.refresh_token.as_deref(), Some("rotated"));
        assert!(state.is_authenticated());

        // The rotated refresh token has to survive the process.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("rotated"), "not persisted: {on_disk}");
    }

    #[tokio::test]
    async fn refresh_keeps_the_old_refresh_token_when_the_server_does_not_rotate() {
        let (endpoint, _server) =
            spawn_token_endpoint(200, r#"{"access_token":"new_access","expires_in":1200}"#);
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        mgr.register(state_against(&endpoint, Some("keep_me"), 60));

        mgr.refresh("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap();

        let state = mgr.get("http://mcp.example").unwrap();
        assert_eq!(state.refresh_token.as_deref(), Some("keep_me"));
    }

    #[tokio::test]
    async fn refresh_without_a_refresh_token_says_so() {
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        mgr.register(state_against("http://127.0.0.1:1/token", None, 60));

        let error = mgr
            .refresh("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, OAuthError::NoRefreshToken { .. }),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn refresh_reports_a_rejected_token_endpoint() {
        let (endpoint, _server) = spawn_token_endpoint(400, r#"{"error":"invalid_grant"}"#);
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        mgr.register(state_against(&endpoint, Some("stale"), 60));

        let error = mgr
            .refresh("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, OAuthError::Rejected { status: 400, .. }),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn refresh_if_needed_leaves_a_fresh_token_alone() {
        // Port 1 is closed: if the refresh path ran, this would fail loudly.
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        mgr.register(state_against("http://127.0.0.1:1/token", Some("r"), 3600));

        let token = mgr
            .refresh_if_needed("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(token.as_deref(), Some("old_access"));
    }

    #[tokio::test]
    async fn refresh_if_needed_refreshes_a_token_inside_the_expiry_window() {
        let (endpoint, server) = spawn_token_endpoint(
            200,
            r#"{"access_token":"refreshed","refresh_token":"r2","expires_in":3600}"#,
        );
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        // 60 seconds left: inside the 5-minute window, so is_near_expiry() is true.
        mgr.register(state_against(&endpoint, Some("r1"), 60));
        assert!(mgr.get("http://mcp.example").unwrap().is_near_expiry());

        let token = mgr
            .refresh_if_needed("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(token.as_deref(), Some("refreshed"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn refresh_if_needed_returns_none_for_an_expired_token_with_no_refresh_token() {
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        mgr.register(state_against("http://127.0.0.1:1/token", None, -60));

        let token = mgr
            .refresh_if_needed("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap();
        assert!(token.is_none());
    }

    #[tokio::test]
    async fn refresh_if_needed_falls_back_to_a_still_valid_token_when_refresh_fails() {
        let (endpoint, _server) = spawn_token_endpoint(500, r#"{"error":"boom"}"#);
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        // Near expiry but not yet expired, and the endpoint is broken.
        mgr.register(state_against(&endpoint, Some("r1"), 60));

        let token = mgr
            .refresh_if_needed("http://mcp.example", &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(token.as_deref(), Some("old_access"));
    }

    /// Read one complete HTTP request (head plus `Content-Length` body).
    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
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

    /// Serve a fixed sequence of responses, one connection each, and return
    /// the requests that arrived. The response bodies are built after the
    /// listener is bound so they can name its address.
    fn spawn_http_sequence<F>(make_responses: F) -> (String, std::thread::JoinHandle<Vec<String>>)
    where
        F: FnOnce(&str) -> Vec<(u16, String)> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let responses = make_responses(&base);
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_http_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
            requests
        });
        (base, handle)
    }

    #[tokio::test]
    async fn discovery_reads_the_authorization_server_metadata_and_persists_it() {
        let metadata = r#"{
            "authorization_endpoint": "https://auth.example/authorize",
            "token_endpoint": "https://auth.example/token"
        }"#;
        let (base, server) = spawn_http_sequence(move |_| vec![(200, metadata.to_string())]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let mut mgr = OAuthManager::with_path(&path);
        let server_url = format!("{base}/mcp");
        mgr.register(McpOAuthState::new(&server_url, "client1"));

        let discovery = mgr
            .discover(&server_url, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(
            discovery.authorization_endpoint.as_deref(),
            Some("https://auth.example/authorize")
        );
        assert_eq!(
            discovery.token_endpoint.as_deref(),
            Some("https://auth.example/token")
        );

        let state = mgr.get(&server_url).unwrap();
        assert_eq!(
            state.authorization_endpoint(),
            "https://auth.example/authorize"
        );
        assert_eq!(state.token_endpoint(), "https://auth.example/token");

        // The cache has to survive the process, or every restart asks again.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("https://auth.example/token"), "{on_disk}");
        assert!(
            on_disk.contains("https://auth.example/authorize"),
            "{on_disk}"
        );

        // The origin was used, not the MCP endpoint path.
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("GET /.well-known/oauth-authorization-server "),
            "unexpected discovery request: {}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn discovery_falls_back_to_openid_configuration() {
        let openid = r#"{
            "issuer": "https://auth.example",
            "authorization_endpoint": "https://openid.example/authorize",
            "token_endpoint": "https://openid.example/token"
        }"#;
        let (base, server) =
            spawn_http_sequence(move |_| vec![(404, String::new()), (200, openid.to_string())]);
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        let server_url = format!("{base}/mcp");

        let discovery = mgr
            .discover(&server_url, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(
            discovery.token_endpoint.as_deref(),
            Some("https://openid.example/token")
        );

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].starts_with("GET /.well-known/oauth-authorization-server "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("GET /.well-known/openid-configuration "),
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn refresh_uses_the_discovered_token_endpoint() {
        let (base, server) = spawn_http_sequence(|base| {
            vec![
                (
                    200,
                    format!(
                        r#"{{"authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/custom/token"}}"#
                    ),
                ),
                (
                    200,
                    r#"{"access_token":"discovered_access","expires_in":3600}"#.to_string(),
                ),
            ]
        });
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        let server_url = format!("{base}/mcp");
        let mut state = McpOAuthState::new(&server_url, "client1");
        state.access_token = Some("old".into());
        state.refresh_token = Some("refresh_me".into());
        state.expires_at = Some(chrono::Utc::now().timestamp() + 60);
        mgr.register(state);

        let token = mgr
            .refresh(&server_url, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(token, "discovered_access");

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].starts_with("POST /custom/token "),
            "the conventional endpoint was used instead of the discovered one: {}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn discovery_failure_is_reported_rather_than_invented() {
        let (base, server) =
            spawn_http_sequence(|_| vec![(404, String::new()), (404, String::new())]);
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));

        let error = mgr
            .discover(&format!("{base}/mcp"), &reqwest::Client::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, OAuthError::Discovery { .. }),
            "unexpected error: {error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn token_endpoint_defaults_to_the_conventional_path() {
        let mut state = McpOAuthState::new("http://mcp.example/", "client1");
        assert_eq!(state.token_endpoint(), "http://mcp.example/oauth/token");
        state.token_endpoint = Some("http://elsewhere/token".into());
        assert_eq!(state.token_endpoint(), "http://elsewhere/token");
    }
}
