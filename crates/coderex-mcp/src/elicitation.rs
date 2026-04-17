//! MCP elicitation — forwarding user prompts/questions from MCP servers.

use serde::{Deserialize, Serialize};

/// An elicitation request from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationRequest {
    /// Unique request ID.
    pub id: String,
    /// The server that sent the request.
    pub server: String,
    /// The message/question for the user.
    pub message: String,
    /// Optional suggested responses.
    #[serde(default)]
    pub suggested_responses: Vec<String>,
}

/// A response to an elicitation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    /// The request ID this responds to.
    pub request_id: String,
    /// Whether the user approved/provided a response.
    pub approved: bool,
    /// The user's response text (if any).
    pub text: Option<String>,
}

/// Elicitation manager — pending requests and response routing.
#[derive(Debug, Default)]
pub struct ElicitationManager {
    pending: Vec<ElicitationRequest>,
    responses: Vec<ElicitationResponse>,
}

impl ElicitationManager {
    /// Create a new elicitation manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            responses: Vec::new(),
        }
    }

    /// Submit an elicitation request.
    pub fn request(&mut self, req: ElicitationRequest) {
        self.pending.push(req);
    }

    /// Get pending requests.
    #[must_use]
    pub fn pending(&self) -> &[ElicitationRequest] {
        &self.pending
    }

    /// Respond to a pending request.
    pub fn respond(&mut self, request_id: &str, approved: bool, text: Option<String>) -> bool {
        let idx = self.pending.iter().position(|r| r.id == request_id);
        if let Some(idx) = idx {
            self.pending.remove(idx);
            self.responses.push(ElicitationResponse {
                request_id: request_id.to_string(),
                approved,
                text,
            });
            true
        } else {
            false
        }
    }

    /// Get a response by request ID.
    #[must_use]
    pub fn get_response(&self, request_id: &str) -> Option<&ElicitationResponse> {
        self.responses.iter().find(|r| r.request_id == request_id)
    }

    /// Number of pending requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Clear all pending and responded requests.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.responses.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> ElicitationRequest {
        ElicitationRequest {
            id: "req1".into(),
            server: "filesystem".into(),
            message: "Allow access to /tmp?".into(),
            suggested_responses: vec!["Yes".into(), "No".into()],
        }
    }

    #[test]
    fn new_is_empty() {
        let mgr = ElicitationManager::new();
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.pending().is_empty());
    }

    #[test]
    fn request_and_respond() {
        let mut mgr = ElicitationManager::new();
        mgr.request(sample_request());
        assert_eq!(mgr.pending_count(), 1);

        let ok = mgr.respond("req1", true, Some("Yes, allow".into()));
        assert!(ok);
        assert_eq!(mgr.pending_count(), 0);

        let resp = mgr.get_response("req1").unwrap();
        assert!(resp.approved);
        assert_eq!(resp.text, Some("Yes, allow".into()));
    }

    #[test]
    fn respond_to_nonexistent_returns_false() {
        let mut mgr = ElicitationManager::new();
        let ok = mgr.respond("nope", true, None);
        assert!(!ok);
    }

    #[test]
    fn deny_response() {
        let mut mgr = ElicitationManager::new();
        mgr.request(sample_request());
        mgr.respond("req1", false, Some("No".into()));
        let resp = mgr.get_response("req1").unwrap();
        assert!(!resp.approved);
    }

    #[test]
    fn clear_removes_all() {
        let mut mgr = ElicitationManager::new();
        mgr.request(sample_request());
        mgr.respond("req1", true, None);
        mgr.clear();
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.get_response("req1").is_none());
    }
}
