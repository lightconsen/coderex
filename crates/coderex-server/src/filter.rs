//! Remote command filtering — remote-safe and bridge-safe filters.

/// Filter commands based on client capabilities.
#[derive(Debug)]
pub struct CommandFilter;

impl CommandFilter {
    /// Check if a command is safe for remote clients.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn is_remote_safe(_command: &str) -> bool {
        // TODO: check against allowlist
        false
    }

    /// Check if a command is safe for bridge sessions.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn is_bridge_safe(_command: &str) -> bool {
        // TODO: check against allowlist
        false
    }
}
