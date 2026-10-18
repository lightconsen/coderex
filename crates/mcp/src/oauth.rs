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
    /// The scopes the authorization server advertised as supported.
    ///
    /// Cached from discovery so a later `McpAuth` call, or the refresh path,
    /// does not have to ask again while the metadata is still fresh.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    /// When the cached discovery metadata (endpoints and scopes) expires,
    /// in epoch seconds.
    ///
    /// Derived from the metadata document's `Cache-Control: max-age`, with a
    /// one-hour default. `None` means no metadata has been cached yet, and
    /// discovery will fetch. A cached document is not re-fetched before this
    /// time.
    #[serde(default)]
    pub discovery_expires_at: Option<i64>,
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
            scopes_supported: Vec::new(),
            discovery_expires_at: None,
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

    /// The discovery metadata cached by a previous run, if it is still fresh.
    ///
    /// The cache is the persisted `authorization_endpoint`, `token_endpoint`
    /// and `scopes_supported` plus [`Self::discovery_expires_at`]. A state with
    /// no expiry, an expired one, or no endpoint has nothing to reuse.
    #[must_use]
    pub fn cached_discovery(&self) -> Option<OAuthDiscovery> {
        let expires_at = self.discovery_expires_at?;
        if chrono::Utc::now().timestamp() >= expires_at {
            return None;
        }
        if self.authorization_endpoint.is_none() && self.token_endpoint.is_none() {
            return None;
        }
        Some(OAuthDiscovery {
            issuer: None,
            authorization_endpoint: self.authorization_endpoint.clone(),
            token_endpoint: self.token_endpoint.clone(),
            scopes_supported: self.scopes_supported.clone(),
        })
    }

    /// Record freshly discovered metadata and when it should be fetched again.
    ///
    /// Endpoints that the document omits are left as they were, so an older
    /// document cannot erase a working endpoint.
    fn record_discovery(&mut self, discovery: &OAuthDiscovery, max_age: u64) {
        if discovery.authorization_endpoint.is_some() {
            self.authorization_endpoint
                .clone_from(&discovery.authorization_endpoint);
        }
        if discovery.token_endpoint.is_some() {
            self.token_endpoint.clone_from(&discovery.token_endpoint);
        }
        self.scopes_supported
            .clone_from(&discovery.scopes_supported);
        let now = chrono::Utc::now().timestamp();
        let ttl = i64::try_from(max_age).unwrap_or(i64::MAX);
        self.discovery_expires_at = Some(now.saturating_add(ttl));
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

/// Metadata an authorization server advertises (RFC 8414 or OpenID Connect).
///
/// The endpoints are optional in the document, but a document that advertises
/// neither is not useful and is treated as a failed discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthDiscovery {
    /// The issuer this document describes.
    ///
    /// Checked against the issuer we asked about: a document for a different
    /// issuer is not usable, even if it was served from this host.
    #[serde(default)]
    pub issuer: Option<String>,
    /// Where the user is sent to approve the client.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    /// Where an authorization code or refresh token is exchanged.
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// Scopes the authorization server supports.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

impl OAuthDiscovery {
    fn is_empty(&self) -> bool {
        self.authorization_endpoint.is_none() && self.token_endpoint.is_none()
    }
}

/// One hour is the default when the server sends no usable `max-age`.
const DEFAULT_DISCOVERY_TTL_SECONDS: u64 = 3600;

/// Normalize an issuer for discovery and comparison.
///
/// Query and fragment are irrelevant to identity, and a trailing slash is not
/// part of it. The path is kept: `https://id.example/realms/demo` and
/// `https://id.example` are different issuers.
fn normalize_issuer(value: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(value).ok()?;
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
fn allows_plain_http(url: &reqwest::Url) -> bool {
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
/// `/.well-known/openid-configuration` *after* it. Both forms are tried, and
/// the origin-level documents remain as a fallback for issuers that publish
/// them there. Duplicates are removed so an issuer without a path does not
/// probe the same URL twice.
fn discovery_candidates(issuer: &str) -> Result<Vec<String>, String> {
    let normalized = normalize_issuer(issuer)
        .ok_or_else(|| format!("{issuer} is not a usable http(s) issuer"))?;
    let url = reqwest::Url::parse(&normalized).map_err(|error| error.to_string())?;
    if !allows_plain_http(&url) {
        return Err(format!(
            "{normalized}: discovery requires https, except for localhost"
        ));
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
    Ok(unique)
}

/// How long a metadata document may be reused, per `Cache-Control`.
///
/// `max-age` is the only directive that matters here; anything else (including
/// a missing header) falls back to one hour. `no-store` and `no-cache` mean
/// "fetch again next time", which is a zero-second TTL.
fn cache_max_age(headers: &reqwest::header::HeaderMap) -> u64 {
    let Some(value) = headers
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
    else {
        return DEFAULT_DISCOVERY_TTL_SECONDS;
    };
    for directive in value.split(',') {
        let directive = directive.trim();
        if directive.eq_ignore_ascii_case("no-store") || directive.eq_ignore_ascii_case("no-cache")
        {
            return 0;
        }
        let Some((name, raw)) = directive.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("max-age") {
            if let Ok(seconds) = raw.trim().trim_matches('"').parse::<u64>() {
                return seconds;
            }
        }
    }
    DEFAULT_DISCOVERY_TTL_SECONDS
}

/// Whether a fetched document describes the issuer we asked about.
///
/// The comparison is normalized exact match: a trailing slash or a query
/// string does not make a different issuer, but a different host or path does.
/// A document with no `issuer` is accepted for compatibility with servers that
/// publish only endpoint fields; RFC 8414 requires the field, so servers that
/// follow the RFC are still checked.
fn issuer_matches(discovery: &OAuthDiscovery, requested: &str) -> bool {
    discovery.issuer.as_deref().map_or(true, |advertised| {
        normalize_issuer(advertised).as_deref() == Some(requested)
    })
}

struct FetchedDiscovery {
    discovery: OAuthDiscovery,
    max_age: u64,
}

async fn fetch_discovery(
    client: &reqwest::Client,
    url: &str,
) -> Result<FetchedDiscovery, String> {
    let response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let max_age = cache_max_age(response.headers());
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let discovery =
        serde_json::from_str(&body).map_err(|error| format!("invalid JSON: {error}"))?;
    Ok(FetchedDiscovery { discovery, max_age })
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
    /// The issuer is the server URL with its query, fragment and trailing slash
    /// removed; its path is load-bearing. Four documents are tried in order:
    ///
    /// 1. `{origin}/.well-known/oauth-authorization-server{path}` — RFC 8414
    ///    path insertion, the Keycloak/Auth0 shape.
    /// 2. `{origin}/.well-known/oauth-authorization-server` — issuer at origin.
    /// 3. `{path}/.well-known/openid-configuration` — OpenID Connect appends to
    ///    the issuer.
    /// 4. `{origin}/.well-known/openid-configuration` — OpenID Connect at
    ///    origin.
    ///
    /// A document whose `issuer` does not match the normalized issuer is
    /// rejected and the next candidate is tried. HTTPS is required, except for
    /// loopback hosts (plain HTTP is allowed there for local development).
    ///
    /// Discovered endpoints and `scopes_supported` are written into the state
    /// for `server_url` (created with the conventional client id if the server
    /// has not been registered yet) and persisted together with an expiry
    /// derived from `Cache-Control: max-age` (one hour by default). A cached
    /// document is returned without a request until that expiry.
    ///
    /// Callers that cannot use `Result` — [`OAuthManager::refresh`] — treat a
    /// failure as "no metadata" and fall back to the conventional paths.
    pub async fn discover(
        &mut self,
        server_url: &str,
        client: &reqwest::Client,
    ) -> Result<OAuthDiscovery, OAuthError> {
        if let Some(cached) = self.get(server_url).and_then(McpOAuthState::cached_discovery) {
            return Ok(cached);
        }

        let normalized = normalize_issuer(server_url).ok_or_else(|| OAuthError::Discovery {
            server_url: server_url.to_string(),
            detail: "the server URL has no usable http(s) issuer".to_string(),
        })?;
        let candidates =
            discovery_candidates(&normalized).map_err(|detail| OAuthError::Discovery {
                server_url: server_url.to_string(),
                detail,
            })?;

        let mut attempts = Vec::new();
        let mut found = None;
        for url in candidates {
            match fetch_discovery(client, &url).await {
                Ok(fetched) if fetched.discovery.is_empty() => {
                    attempts.push(format!("{url}: advertised no endpoints"));
                }
                Ok(fetched) if !issuer_matches(&fetched.discovery, &normalized) => {
                    attempts.push(format!(
                        "{url}: issuer {:?} does not match {normalized}",
                        fetched.discovery.issuer
                    ));
                }
                Ok(fetched) => {
                    found = Some(fetched);
                    break;
                }
                Err(error) => attempts.push(format!("{url}: {error}")),
            }
        }

        let Some(fetched) = found else {
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
        state.record_discovery(&fetched.discovery, fetched.max_age);

        self.save_tokens().map_err(|source| OAuthError::Store {
            path: self.path.display().to_string(),
            source,
        })?;

        Ok(fetched.discovery)
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

    struct HttpReply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl HttpReply {
        fn new(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_string(), value.to_string()));
            self
        }
    }

    /// Serve a fixed sequence of replies, one connection each, and return the
    /// requests that arrived. The bodies are built after the listener is bound
    /// so they can name its address.
    fn spawn_http_replies<F>(make_responses: F) -> (String, std::thread::JoinHandle<Vec<String>>)
    where
        F: FnOnce(&str) -> Vec<HttpReply> + Send + 'static,
    {
        use std::fmt::Write as _;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let responses = make_responses(&base);
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in responses {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_http_request(&mut stream));
                let mut response = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\n",
                    reply.status
                );
                for (name, value) in &reply.headers {
                    let _ = write!(response, "{name}: {value}\r\n");
                }
                let _ = write!(
                    response,
                    "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.body.len(),
                    reply.body
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
            requests
        });
        (base, handle)
    }

    fn spawn_http_sequence<F>(make_responses: F) -> (String, std::thread::JoinHandle<Vec<String>>)
    where
        F: FnOnce(&str) -> Vec<(u16, String)> + Send + 'static,
    {
        spawn_http_replies(move |base| {
            make_responses(base)
                .into_iter()
                .map(|(status, body)| HttpReply::new(status, body))
                .collect()
        })
    }

    #[test]
    fn issuer_normalization_keeps_the_path_and_drops_the_rest() {
        assert_eq!(
            normalize_issuer("https://id.example/realms/demo/?foo=bar#frag").as_deref(),
            Some("https://id.example/realms/demo")
        );
        assert_eq!(
            normalize_issuer("https://id.example/").as_deref(),
            Some("https://id.example")
        );
        assert_eq!(normalize_issuer("not a url"), None);
        assert_eq!(normalize_issuer("ftp://id.example/realms/demo"), None);
    }

    #[test]
    fn discovery_candidates_put_path_insertion_before_origin_documents() {
        let candidates =
            discovery_candidates("https://id.example/realms/demo/?ignored=1#frag").unwrap();
        assert_eq!(
            candidates,
            vec![
                "https://id.example/.well-known/oauth-authorization-server/realms/demo",
                "https://id.example/.well-known/oauth-authorization-server",
                "https://id.example/realms/demo/.well-known/openid-configuration",
                "https://id.example/.well-known/openid-configuration",
            ]
        );

        // Without a path the path-insertion and origin documents are the same
        // URL; it must still be probed only once.
        let origin_only = discovery_candidates("https://id.example").unwrap();
        assert_eq!(
            origin_only,
            vec![
                "https://id.example/.well-known/oauth-authorization-server",
                "https://id.example/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn discovery_rejects_plain_http_outside_localhost() {
        let error = discovery_candidates("http://id.example/realms/demo").unwrap_err();
        assert!(error.contains("requires https"), "{error}");
        assert!(discovery_candidates("http://127.0.0.1:8080/realms/demo").is_ok());
        assert!(discovery_candidates("http://localhost:8080/realms/demo").is_ok());
    }

    #[test]
    fn cache_control_max_age_defaults_to_one_hour() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(cache_max_age(&headers), 3600);
        headers.insert(
            reqwest::header::CACHE_CONTROL,
            "public, max-age=120".parse().unwrap(),
        );
        assert_eq!(cache_max_age(&headers), 120);
        headers.insert(reqwest::header::CACHE_CONTROL, "no-store".parse().unwrap());
        assert_eq!(cache_max_age(&headers), 0);
    }

    #[tokio::test]
    async fn discovery_uses_rfc_8414_path_insertion_and_persists_the_cache() {
        let (base, server) = spawn_http_replies(|base| {
            vec![HttpReply::new(
                200,
                format!(
                    r#"{{"issuer":"{base}/realms/demo/","authorization_endpoint":"https://auth.example/authorize","token_endpoint":"https://auth.example/token","scopes_supported":["openid","mcp"]}}"#
                ),
            )
            .with_header("cache-control", "public, max-age=120")]
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_tokens.json");
        let mut mgr = OAuthManager::with_path(&path);
        // The trailing slash on the issuer must not change identity.
        let server_url = format!("{base}/realms/demo/");
        mgr.register(McpOAuthState::new(&server_url, "client1"));
        let before = chrono::Utc::now().timestamp();

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
        assert_eq!(discovery.scopes_supported, vec!["openid", "mcp"]);

        let state = mgr.get(&server_url).unwrap();
        assert_eq!(
            state.authorization_endpoint(),
            "https://auth.example/authorize"
        );
        assert_eq!(state.token_endpoint(), "https://auth.example/token");
        assert_eq!(state.scopes_supported, vec!["openid", "mcp"]);
        let expires_at = state.discovery_expires_at.unwrap();
        assert!(
            (before + 100..=before + 125).contains(&expires_at),
            "the Cache-Control max-age was not used: {expires_at} vs {before}"
        );

        // The cache has to survive the process, or every restart asks again.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("https://auth.example/token"), "{on_disk}");
        assert!(
            on_disk.contains("https://auth.example/authorize"),
            "{on_disk}"
        );
        assert!(on_disk.contains("scopes_supported"), "{on_disk}");
        assert!(on_disk.contains("discovery_expires_at"), "{on_disk}");

        // Path insertion came first, and the issuer path was kept.
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .starts_with("GET /.well-known/oauth-authorization-server/realms/demo "),
            "unexpected discovery request: {}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn discovery_rejects_a_mismatched_issuer_and_continues() {
        let (base, server) = spawn_http_sequence(|base| {
            vec![
                (
                    200,
                    r#"{"issuer":"https://evil.example","authorization_endpoint":"https://evil.example/authorize","token_endpoint":"https://evil.example/token"}"#.to_string(),
                ),
                (
                    200,
                    format!(
                        r#"{{"issuer":"{base}/realms/demo","authorization_endpoint":"https://good.example/authorize","token_endpoint":"https://good.example/token"}}"#
                    ),
                ),
            ]
        });
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        let server_url = format!("{base}/realms/demo");

        let discovery = mgr
            .discover(&server_url, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(
            discovery.token_endpoint.as_deref(),
            Some("https://good.example/token"),
            "the mismatched document was accepted"
        );

        // The second document is the origin-level RFC 8414 path.
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].starts_with("GET /.well-known/oauth-authorization-server "),
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn discovery_falls_back_to_openid_configuration_at_the_issuer_path() {
        let (base, server) = spawn_http_sequence(|base| {
            vec![
                (404, String::new()),
                (404, String::new()),
                (
                    200,
                    format!(
                        r#"{{"issuer":"{base}/realms/demo","authorization_endpoint":"https://openid.example/authorize","token_endpoint":"https://openid.example/token"}}"#
                    ),
                ),
            ]
        });
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        let server_url = format!("{base}/realms/demo");

        let discovery = mgr
            .discover(&server_url, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(
            discovery.token_endpoint.as_deref(),
            Some("https://openid.example/token")
        );

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]
                .starts_with("GET /.well-known/oauth-authorization-server/realms/demo "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("GET /.well-known/oauth-authorization-server "),
            "{}",
            requests[1]
        );
        assert!(
            requests[2].starts_with("GET /realms/demo/.well-known/openid-configuration "),
            "{}",
            requests[2]
        );
    }

    #[tokio::test]
    async fn discovery_reuses_fresh_cached_metadata_without_a_request() {
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        // Port 1 is closed: any fetch attempt fails, so Ok proves the cache
        // path was taken.
        let server_url = "http://127.0.0.1:1/realms/demo";
        let mut state = McpOAuthState::new(server_url, "client1");
        state.authorization_endpoint = Some("https://id.example/auth".into());
        state.token_endpoint = Some("https://id.example/token".into());
        state.scopes_supported = vec!["openid".into(), "mcp".into()];
        state.discovery_expires_at = Some(chrono::Utc::now().timestamp() + 3600);
        mgr.register(state);

        let discovery = mgr
            .discover(server_url, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(
            discovery.authorization_endpoint.as_deref(),
            Some("https://id.example/auth")
        );
        assert_eq!(
            discovery.token_endpoint.as_deref(),
            Some("https://id.example/token")
        );
        assert_eq!(discovery.scopes_supported, vec!["openid", "mcp"]);
    }

    #[tokio::test]
    async fn an_expired_cache_is_not_used() {
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        let server_url = "http://127.0.0.1:1/realms/demo";
        let mut state = McpOAuthState::new(server_url, "client1");
        state.token_endpoint = Some("https://id.example/token".into());
        state.discovery_expires_at = Some(chrono::Utc::now().timestamp() - 1);
        mgr.register(state);

        let error = mgr
            .discover(server_url, &reqwest::Client::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, OAuthError::Discovery { .. }),
            "an expired cache was reused: {error}"
        );
    }

    #[tokio::test]
    async fn refresh_uses_the_discovered_token_endpoint() {
        let (base, server) = spawn_http_sequence(|base| {
            vec![
                (
                    200,
                    format!(
                        r#"{{"issuer":"{base}/mcp","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/custom/token"}}"#
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
    async fn a_failed_discovery_leaves_the_conventional_endpoints() {
        let (base, server) = spawn_http_sequence(|_| {
            vec![
                (404, String::new()),
                (404, String::new()),
                (404, String::new()),
                (404, String::new()),
            ]
        });
        let mut mgr = OAuthManager::with_path(tempfile::tempdir().unwrap().path().join("t.json"));
        let server_url = format!("{base}/mcp");
        mgr.register(McpOAuthState::new(&server_url, "client1"));

        let error = mgr
            .discover(&server_url, &reqwest::Client::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, OAuthError::Discovery { .. }),
            "unexpected error: {error}"
        );

        // Nothing was published, so the conventional path remains in effect.
        assert_eq!(
            mgr.get(&server_url).unwrap().token_endpoint(),
            format!("{server_url}/oauth/token")
        );

        // All four candidates were tried, in precedence order.
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(
            requests[0]
                .starts_with("GET /.well-known/oauth-authorization-server/mcp "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("GET /.well-known/oauth-authorization-server "),
            "{}",
            requests[1]
        );
        assert!(
            requests[2].starts_with("GET /mcp/.well-known/openid-configuration "),
            "{}",
            requests[2]
        );
        assert!(
            requests[3].starts_with("GET /.well-known/openid-configuration "),
            "{}",
            requests[3]
        );
    }

    #[test]
    fn token_endpoint_defaults_to_the_conventional_path() {
        let mut state = McpOAuthState::new("http://mcp.example/", "client1");
        assert_eq!(state.token_endpoint(), "http://mcp.example/oauth/token");
        state.token_endpoint = Some("http://elsewhere/token".into());
        assert_eq!(state.token_endpoint(), "http://elsewhere/token");
    }
}
