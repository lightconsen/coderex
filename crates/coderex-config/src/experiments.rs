//! Remote experiment configuration sources.

use std::collections::HashMap;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Trait for remote experiment configuration sources.
#[async_trait::async_trait]
pub trait ExperimentSource: Send + Sync {
    /// Fetch experiment overrides.
    ///
    /// Returns a map of flag name -> enabled state.
    async fn fetch_experiments(&self) -> Result<HashMap<String, bool>, ExperimentError>;
}

/// Stub implementation that returns empty overrides.
pub struct StubExperimentSource;

#[async_trait::async_trait]
impl ExperimentSource for StubExperimentSource {
    async fn fetch_experiments(&self) -> Result<HashMap<String, bool>, ExperimentError> {
        // TODO: real HTTP fetch from remote config endpoint
        Ok(HashMap::new())
    }
}

/// Cached experiment source that stores results on disk.
///
/// If the cache is older than `max_age`, stale data is returned
/// while fresh data is fetched in the background.
pub struct CachedExperimentSource<S: ExperimentSource> {
    inner: S,
    cache_path: PathBuf,
    max_age: std::time::Duration,
}

impl<S: ExperimentSource> CachedExperimentSource<S> {
    /// Create a new cached experiment source.
    #[must_use]
    pub const fn new(inner: S, cache_path: PathBuf, max_age: std::time::Duration) -> Self {
        Self {
            inner,
            cache_path,
            max_age,
        }
    }

    /// Check if the cached data is still fresh.
    fn is_fresh(&self) -> bool {
        self.cache_path.exists()
            && std::fs::metadata(&self.cache_path)
                .and_then(|m| m.modified())
                .map(|mtime| mtime.elapsed().is_ok_and(|age| age < self.max_age))
                .unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl<S: ExperimentSource + 'static> ExperimentSource for CachedExperimentSource<S> {
    async fn fetch_experiments(&self) -> Result<HashMap<String, bool>, ExperimentError> {
        if self.is_fresh() {
            // Return cached data
            let content = std::fs::read_to_string(&self.cache_path).map_err(|e| {
                ExperimentError::Cache(std::io::Error::other(e))
            })?;
            let cached: CachedData = serde_json::from_str(&content).map_err(|e| {
                ExperimentError::Cache(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            return Ok(cached.experiments);
        }

        // Fetch fresh data
        let experiments = self.inner.fetch_experiments().await?;
        let cached = CachedData {
            experiments: experiments.clone(),
            fetched_at: chrono::Utc::now(),
        };
        if let Ok(json) = serde_json::to_string(&cached) {
            let _ = std::fs::write(&self.cache_path, json);
        }
        Ok(experiments)
    }
}

/// Cached experiment data with timestamp.
#[derive(Debug, Serialize, Deserialize)]
struct CachedData {
    experiments: HashMap<String, bool>,
    fetched_at: chrono::DateTime<chrono::Utc>,
}

/// Errors from experiment fetching.
#[derive(Debug, Error)]
pub enum ExperimentError {
    /// Network error during fetch.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    /// JSON parse error.
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
    /// Cache I/O error.
    #[error("cache error: {0}")]
    Cache(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_source_returns_empty_map() {
        let source = StubExperimentSource;
        let result = source.fetch_experiments().await.unwrap();
        assert!(result.is_empty());
    }
}
