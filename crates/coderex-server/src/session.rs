//! Detached WebSocket session management.

/// A connected WebSocket session.
#[derive(Debug)]
pub struct Session {
    /// The unique session identifier.
    pub id: String,
}

/// Server for managing detached sessions over WebSocket.
#[derive(Debug)]
pub struct DaemonServer;

impl Default for DaemonServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonServer {
    /// Create a new daemon server.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}
