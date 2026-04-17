//! MCP OAuth authentication flow.

use serde::{Deserialize, Serialize};

/// OAuth state for an MCP server connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthState {
    /// The server URL.
    pub server_url: String,
    /// OAuth client ID.
    pub client_id: String,
    /// OAuth client secret (if applicable).
    pub client_secret: Option<String>,
    /// Access token.
    pub access_token: Option<String>,
    /// Refresh token.
    pub refresh_token: Option<String>,
    /// Token expiry timestamp (epoch seconds).
    pub expires_at: Option<i64>,
    /// Scopes granted.
    #[serde(default)]
    pub scopes: Vec<String>,
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
        }
    }

    /// Check if the access token is expired.
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

    /// Check if authenticated.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.access_token.is_some() && !self.is_expired()
    }

    /// Update tokens from an OAuth flow.
    pub fn update_tokens(&mut self, access: String, refresh: Option<String>, expires_in: i64) {
        self.access_token = Some(access);
        self.refresh_token = refresh;
        self.expires_at = Some(chrono::Utc::now().timestamp() + expires_in);
    }
}

/// OAuth flow manager.
#[derive(Debug, Default)]
pub struct OAuthManager {
    states: Vec<McpOAuthState>,
}

impl OAuthManager {
    /// Create a new OAuth manager.
    #[must_use]
    pub fn new() -> Self {
        Self { states: Vec::new() }
    }

    /// Register a server for OAuth.
    pub fn register(&mut self, state: McpOAuthState) {
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
        self.get(server_url).map_or(false, |s| s.is_authenticated())
    }

    /// List all registered server URLs.
    #[must_use]
    pub fn server_urls(&self) -> Vec<&str> {
        self.states.iter().map(|s| s.server_url.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
