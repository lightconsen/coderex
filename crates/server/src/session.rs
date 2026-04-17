//! Detached WebSocket session management with authentication and idle cleanup.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A connected WebSocket session.
#[derive(Debug)]
pub struct Session {
    /// The unique session identifier.
    pub id: String,
    /// The API key or token used for authentication.
    pub auth_token: String,
    /// When the session was created.
    pub created_at: Instant,
    /// Last activity timestamp.
    pub last_activity: Instant,
    /// Whether the session is authenticated.
    pub authenticated: bool,
}

/// Configuration for the daemon server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Valid API keys for authentication.
    pub valid_keys: Vec<String>,
    /// Duration after which idle sessions are cleaned up.
    pub idle_timeout: Duration,
    /// Maximum number of concurrent sessions.
    pub max_sessions: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            valid_keys: Vec::new(),
            idle_timeout: Duration::from_secs(3600),
            max_sessions: 100,
        }
    }
}

/// Server for managing detached sessions over WebSocket.
#[derive(Debug)]
pub struct DaemonServer {
    /// Active sessions.
    sessions: HashMap<String, Session>,
    /// Server configuration.
    config: ServerConfig,
}

impl Default for DaemonServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonServer {
    /// Create a new daemon server with default config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            config: ServerConfig::default(),
        }
    }

    /// Create a new daemon server with the given config.
    #[must_use]
    pub fn with_config(config: ServerConfig) -> Self {
        Self {
            sessions: HashMap::new(),
            config,
        }
    }

    /// Authenticate a new session with the given token.
    pub fn authenticate(&mut self, token: &str) -> Result<String, String> {
        if !self.config.valid_keys.is_empty() && !self.config.valid_keys.iter().any(|k| k == token) {
            return Err("invalid API key".to_string());
        }
        if self.sessions.len() >= self.config.max_sessions {
            return Err("max sessions reached".to_string());
        }

        let id = format!("sess_{}", self.sessions.len() + 1);
        let now = Instant::now();
        self.sessions.insert(id.clone(), Session {
            id: id.clone(),
            auth_token: token.to_string(),
            created_at: now,
            last_activity: now,
            authenticated: true,
        });
        Ok(id)
    }

    /// Record activity for a session (resets idle timer).
    pub fn record_activity(&mut self, session_id: &str) -> bool {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.last_activity = Instant::now();
            true
        } else {
            false
        }
    }

    /// Remove idle sessions that have exceeded the timeout.
    pub fn cleanup_idle(&mut self) -> Vec<String> {
        let now = Instant::now();
        let timeout = self.config.idle_timeout;
        let mut removed = Vec::new();

        self.sessions.retain(|id, session| {
            if now.duration_since(session.last_activity) > timeout {
                removed.push(id.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// List all active sessions.
    #[must_use]
    pub fn list_sessions(&self) -> Vec<&Session> {
        self.sessions.values().collect()
    }

    /// Get a session by ID.
    #[must_use]
    pub fn get_session(&self, session_id: &str) -> Option<&Session> {
        self.sessions.get(session_id)
    }

    /// Disconnect a session.
    pub fn disconnect(&mut self, session_id: &str) -> bool {
        self.sessions.remove(session_id).is_some()
    }

    /// Number of active sessions.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_keys() -> DaemonServer {
        DaemonServer::with_config(ServerConfig {
            valid_keys: vec!["key1".to_string(), "key2".to_string()],
            idle_timeout: Duration::from_secs(1),
            max_sessions: 10,
        })
    }

    #[test]
    fn authenticate_with_valid_key() {
        let mut server = server_with_keys();
        let id = server.authenticate("key1").unwrap();
        assert!(id.starts_with("sess_"));
        let session = server.get_session(&id).unwrap();
        assert!(session.authenticated);
        assert_eq!(session.auth_token, "key1");
    }

    #[test]
    fn authenticate_with_invalid_key() {
        let mut server = server_with_keys();
        let err = server.authenticate("badkey").unwrap_err();
        assert_eq!(err, "invalid API key");
    }

    #[test]
    fn authenticate_no_keys_allows_anything() {
        let mut server = DaemonServer::new();
        let id = server.authenticate("anything").unwrap();
        assert!(server.get_session(&id).is_some());
    }

    #[test]
    fn cleanup_removes_idle_sessions() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            idle_timeout: Duration::from_millis(50),
            max_sessions: 10,
        });
        let id = server.authenticate("tok").unwrap();
        // Wait for timeout
        std::thread::sleep(Duration::from_millis(60));
        let removed = server.cleanup_idle();
        assert_eq!(removed, vec![id]);
        assert_eq!(server.session_count(), 0);
    }

    #[test]
    fn record_activity_prevents_cleanup() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            idle_timeout: Duration::from_millis(50),
            max_sessions: 10,
        });
        let id = server.authenticate("tok").unwrap();
        // Activity right before timeout
        std::thread::sleep(Duration::from_millis(30));
        server.record_activity(&id);
        std::thread::sleep(Duration::from_millis(30));
        let removed = server.cleanup_idle();
        assert!(removed.is_empty());
        assert_eq!(server.session_count(), 1);
    }

    #[test]
    fn max_sessions_enforced() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            idle_timeout: Duration::from_secs(3600),
            max_sessions: 2,
        });
        server.authenticate("a").unwrap();
        server.authenticate("b").unwrap();
        let err = server.authenticate("c").unwrap_err();
        assert_eq!(err, "max sessions reached");
    }

    #[test]
    fn disconnect_removes_session() {
        let mut server = server_with_keys();
        let id = server.authenticate("key1").unwrap();
        assert!(server.disconnect(&id));
        assert!(!server.disconnect(&id)); // already gone
        assert_eq!(server.session_count(), 0);
    }

    #[test]
    fn list_sessions() {
        let mut server = server_with_keys();
        server.authenticate("key1").unwrap();
        server.authenticate("key2").unwrap();
        let sessions = server.list_sessions();
        assert_eq!(sessions.len(), 2);
    }
}
