//! Bridge work poller — environment registration and work assignment.

/// Polls or receives work assignments from the remote control plane.
#[derive(Debug)]
pub struct BridgeWorkPoller;

impl Default for BridgeWorkPoller {
    fn default() -> Self {
        Self::new()
    }
}

impl BridgeWorkPoller {
    /// Create a new work poller.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}
