//! Simple per-tool token-bucket rate limiter.
//!
//! Tracks request timestamps and rejects calls that exceed a per-minute quota.

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Key for `ToolContext.extra` to access the shared rate limiter.
pub const RATE_LIMITER_KEY: &str = "rate_limiter";

/// Per-tool rate limit configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum requests per minute.
    pub max_per_minute: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_per_minute: 30,
        }
    }
}

/// Shared in-memory rate limiter.
#[derive(Debug, Default)]
pub struct RateLimiter {
    /// tool_name -> list of request Instants in the last minute.
    requests: DashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    /// Create a new rate limiter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether a request for `tool_name` is allowed under the given quota.
    ///
    /// Returns `true` if the request should proceed. The timestamp is recorded
    /// immediately so subsequent checks see this call.
    pub fn check(&self, tool_name: &str, max_per_minute: u32) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(60);

        let mut entry = self.requests.entry(tool_name.to_string()).or_default();
        // Remove timestamps outside the 60-second window
        entry.retain(|t| now.duration_since(*t) < window);

        if entry.len() >= max_per_minute as usize {
            false
        } else {
            entry.push(now);
            true
        }
    }
}

/// Convenience: get the rate limiter from `ToolContext.extra`.
pub fn get_rate_limiter(ctx: &crate::tool::ToolContext<'_>) -> Option<Arc<RateLimiter>> {
    ctx.extra
        .get(RATE_LIMITER_KEY)
        .and_then(|v| v.downcast_ref::<Arc<RateLimiter>>().map(Arc::clone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_under_quota() {
        let rl = RateLimiter::new();
        assert!(rl.check("WebFetch", 3));
        assert!(rl.check("WebFetch", 3));
        assert!(rl.check("WebFetch", 3));
    }

    #[test]
    fn rate_limiter_blocks_over_quota() {
        let rl = RateLimiter::new();
        assert!(rl.check("WebFetch", 2));
        assert!(rl.check("WebFetch", 2));
        assert!(!rl.check("WebFetch", 2));
    }

    #[test]
    fn rate_limiter_is_per_tool() {
        let rl = RateLimiter::new();
        assert!(rl.check("WebFetch", 1));
        assert!(!rl.check("WebFetch", 1));
        assert!(rl.check("WebSearch", 1));
    }
}
