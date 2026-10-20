//! Bridge permission router — pending permission store and remote forwarding.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A pending permission request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermission {
    /// Unique request ID.
    pub id: String,
    /// The session this belongs to.
    pub session_id: String,
    /// The tool requesting permission.
    pub tool: String,
    /// Tool arguments.
    pub args: String,
    /// Whether this was already resolved.
    pub resolved: bool,
}

/// Response to a permission request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PermissionResponse {
    /// Permission granted.
    Allow {
        /// Whether to always allow this tool+args combination.
        always: bool,
    },
    /// Permission denied.
    Deny,
}

/// Routes permission requests to the remote client.
#[derive(Debug)]
pub struct BridgePermissionRouter {
    /// Pending permission requests.
    pending: HashMap<String, PendingPermission>,
    /// Completed responses.
    responses: HashMap<String, PermissionResponse>,
    /// Counter for generating unique IDs.
    next_id: usize,
}

impl Default for BridgePermissionRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl BridgePermissionRouter {
    /// Create a new permission router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            responses: HashMap::new(),
            next_id: 0,
        }
    }

    /// Create a new permission request.
    pub fn create_request(&mut self, session_id: String, tool: String, args: String) -> String {
        let id = format!("perm_{}", self.next_id);
        self.next_id += 1;
        self.pending.insert(
            id.clone(),
            PendingPermission {
                id: id.clone(),
                session_id,
                tool,
                args,
                resolved: false,
            },
        );
        id
    }

    /// Look up a pending request.
    #[must_use]
    pub fn get_pending(&self, request_id: &str) -> Option<&PendingPermission> {
        self.pending.get(request_id)
    }

    /// Submit the first response to a permission request.
    ///
    /// The first answer wins. A late answer (for example, one that arrives
    /// after the launcher's bounded approval wait already denied the request)
    /// is refused rather than overwriting the decision the child was given.
    pub fn respond(&mut self, request_id: &str, response: PermissionResponse) -> bool {
        if let Some(req) = self.pending.get_mut(request_id) {
            if req.resolved {
                return false;
            }
            req.resolved = true;
            self.responses.insert(request_id.to_string(), response);
            true
        } else {
            false
        }
    }

    /// Get the response for a request.
    #[must_use]
    pub fn get_response(&self, request_id: &str) -> Option<&PermissionResponse> {
        self.responses.get(request_id)
    }

    /// Check if a request was resolved.
    #[must_use]
    pub fn is_resolved(&self, request_id: &str) -> bool {
        self.pending.get(request_id).is_some_and(|p| p.resolved)
    }

    /// Get all pending (unresolved) requests for a session.
    #[must_use]
    pub fn pending_for_session(&self, session_id: &str) -> Vec<&PendingPermission> {
        self.pending
            .values()
            .filter(|p| p.session_id == session_id && !p.resolved)
            .collect()
    }

    /// Number of pending requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.values().filter(|p| !p.resolved).count()
    }

    /// Clear all resolved requests.
    pub fn prune_resolved(&mut self) {
        self.pending.retain(|_, p| !p.resolved);
        // Keep responses for audit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request() {
        let mut router = BridgePermissionRouter::new();
        let id = router.create_request("s1".into(), "Bash".into(), "ls -la".into());
        assert_eq!(id, "perm_0");
        assert_eq!(router.pending_count(), 1);
    }

    #[test]
    fn respond_to_request() {
        let mut router = BridgePermissionRouter::new();
        let id = router.create_request("s1".into(), "Bash".into(), "ls".into());
        assert!(!router.is_resolved(&id));

        let ok = router.respond(&id, PermissionResponse::Allow { always: false });
        assert!(ok);
        assert!(router.is_resolved(&id));

        if let Some(PermissionResponse::Allow { always }) = router.get_response(&id) {
            assert!(!*always);
        } else {
            panic!("expected Allow response");
        }
    }

    #[test]
    fn respond_to_nonexistent_returns_false() {
        let mut router = BridgePermissionRouter::new();
        assert!(!router.respond("nope", PermissionResponse::Deny));
    }

    #[test]
    fn pending_for_session() {
        let mut router = BridgePermissionRouter::new();
        router.create_request("s1".into(), "Bash".into(), "a".into());
        router.create_request("s2".into(), "Read".into(), "b".into());
        router.create_request("s1".into(), "Write".into(), "c".into());

        let pending = router.pending_for_session("s1");
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn deny_response() {
        let mut router = BridgePermissionRouter::new();
        let id = router.create_request("s1".into(), "Bash".into(), "rm".into());
        router.respond(&id, PermissionResponse::Deny);
        assert!(matches!(
            router.get_response(&id),
            Some(PermissionResponse::Deny)
        ));
    }

    #[test]
    fn a_late_response_cannot_overwrite_the_first_one() {
        let mut router = BridgePermissionRouter::new();
        let id = router.create_request("s1".into(), "Bash".into(), "ls".into());

        assert!(router.respond(&id, PermissionResponse::Deny));
        // A gateway answer that arrives after the approval timed out is
        // refused: the child has already been told no.
        assert!(!router.respond(&id, PermissionResponse::Allow { always: true }));
        assert!(matches!(
            router.get_response(&id),
            Some(PermissionResponse::Deny)
        ));
    }

    #[test]
    fn prune_resolved() {
        let mut router = BridgePermissionRouter::new();
        let id = router.create_request("s1".into(), "Bash".into(), "a".into());
        router.respond(&id, PermissionResponse::Allow { always: true });
        assert_eq!(router.pending_count(), 0);
        assert_eq!(router.pending.len(), 1); // still in map but resolved
        router.prune_resolved();
        assert!(router.pending.is_empty());
        // Response is kept
        assert!(router.get_response(&id).is_some());
    }
}
