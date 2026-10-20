//! Global tool result cache with TTL and file mtime invalidation.
//!
//! Cached entries are keyed by `(tool_name, arguments_hash)`.  Only
//! read-only tools are cached.  The `Read` tool additionally stores the
//! file's mtime so that edits invalidate the cache immediately.

use crate::context::ToolOutput;
use moka::future::Cache;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, SystemTime};

/// Configuration for the tool result cache.
#[derive(Debug, Clone)]
pub struct ToolCacheConfig {
    /// Whether caching is enabled.
    pub enabled: bool,
    /// Default TTL for cached entries.
    pub default_ttl: Duration,
    /// Maximum number of cached entries.
    pub max_capacity: u64,
}

impl Default for ToolCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_ttl: Duration::from_secs(300), // 5 minutes
            max_capacity: 1000,
        }
    }
}

/// A single cached tool result.
#[derive(Debug, Clone)]
pub struct CachedOutput {
    /// The tool output.
    pub output: ToolOutput,
    /// For file-read tools: the path that was read.
    pub file_path: Option<String>,
    /// For file-read tools: the mtime at cache time.
    pub file_mtime: Option<SystemTime>,
}

/// Global LRU cache for tool results.
#[derive(Debug, Clone)]
pub struct ToolCache {
    inner: Cache<String, CachedOutput>,
    config: ToolCacheConfig,
}

impl ToolCache {
    /// Create a new cache with the given config.
    pub fn new(config: ToolCacheConfig) -> Self {
        let inner = Cache::builder()
            .max_capacity(config.max_capacity)
            .time_to_live(config.default_ttl)
            .build();
        Self { inner, config }
    }

    /// Build the cache key for a tool call.
    fn cache_key(tool_name: &str, arguments: &Value) -> String {
        let mut hasher = DefaultHasher::new();
        arguments.hash(&mut hasher);
        let hash = hasher.finish();
        format!("{tool_name}:{hash}")
    }

    /// Check whether a result is in the cache and still valid.
    ///
    /// For `Read` tool results, also checks that the file's mtime has not
    /// changed since the entry was created.
    pub async fn get(&self, tool_name: &str, arguments: &Value) -> Option<ToolOutput> {
        if !self.config.enabled {
            return None;
        }

        let key = Self::cache_key(tool_name, arguments);
        let entry = self.inner.get(&key).await?;

        // TTL is handled by moka, but we still need mtime checks
        if tool_name == "Read" {
            if let Some(ref path_str) = entry.file_path {
                let current_mtime = std::fs::metadata(path_str).and_then(|m| m.modified()).ok();
                if current_mtime != entry.file_mtime {
                    // File changed — invalidate
                    self.inner.invalidate(&key).await;
                    return None;
                }
            }
        }

        Some(entry.output)
    }

    /// Insert a tool result into the cache.
    ///
    /// Only read-only tools should be passed here; the caller is expected
    /// to filter.
    pub async fn insert(&self, tool_name: &str, arguments: &Value, output: ToolOutput) {
        if !self.config.enabled {
            return;
        }

        let (file_path, file_mtime) = if tool_name == "Read" {
            arguments
                .get("file_path")
                .and_then(Value::as_str)
                .map_or((None, None), |path| {
                    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                    (Some(path.to_string()), mtime)
                })
        } else {
            (None, None)
        };

        let key = Self::cache_key(tool_name, arguments);
        let entry = CachedOutput {
            output,
            file_path,
            file_mtime,
        };
        self.inner.insert(key, entry).await;
    }

    /// Invalidate all entries for a given tool name.
    pub async fn invalidate_tool(&self, tool_name: &str) {
        if !self.config.enabled {
            return;
        }
        // moka doesn't support prefix-based invalidation directly,
        // so we iterate all keys.  This is acceptable for cache maintenance.
        let prefix = format!("{tool_name}:");
        let keys_to_remove: Vec<String> = self
            .inner
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| (*k).clone())
            .collect();
        for key in keys_to_remove {
            self.inner.invalidate(&key).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_output(text: &str) -> ToolOutput {
        ToolOutput {
            output: text.to_string(),
            is_display: true,
            images: Vec::new(),
        }
    }

    #[tokio::test]
    async fn cache_hits_and_misses() {
        let cache = ToolCache::new(ToolCacheConfig::default());
        let args = serde_json::json!({"query": "rust"});

        // Miss
        assert!(cache.get("WebSearch", &args).await.is_none());

        // Insert
        cache
            .insert("WebSearch", &args, test_output("results"))
            .await;

        // Hit
        let hit = cache.get("WebSearch", &args).await.unwrap();
        assert_eq!(hit.output, "results");
    }

    #[tokio::test]
    async fn cache_is_per_tool() {
        let cache = ToolCache::new(ToolCacheConfig::default());
        let args = serde_json::json!({"query": "rust"});

        cache.insert("WebSearch", &args, test_output("web")).await;

        // Same args, different tool = miss
        assert!(cache.get("Skill", &args).await.is_none());
    }

    #[tokio::test]
    async fn disabled_cache_always_misses() {
        let cache = ToolCache::new(ToolCacheConfig {
            enabled: false,
            ..Default::default()
        });
        let args = serde_json::json!({"query": "rust"});

        cache
            .insert("WebSearch", &args, test_output("results"))
            .await;
        assert!(cache.get("WebSearch", &args).await.is_none());
    }
}
