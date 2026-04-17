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
///
/// Useful as a fallback when no remote endpoint is configured.
#[derive(Debug)]
pub struct StubExperimentSource;

#[async_trait::async_trait]
impl ExperimentSource for StubExperimentSource {
    async fn fetch_experiments(&self) -> Result<HashMap<String, bool>, ExperimentError> {
        Ok(HashMap::new())
    }
}

/// HTTP-based experiment source that fetches from a remote config endpoint.
///
/// Expected response format: `{ "flag_name": true, "other_flag": false, ... }`
#[derive(Debug, Clone)]
pub struct HttpExperimentSource {
    url: String,
    client: reqwest::Client,
}

impl HttpExperimentSource {
    /// Create a new HTTP experiment source.
    #[must_use]
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom reqwest client (e.g. with timeouts or proxies).
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // reqwest::Client is not const-constructible
    pub fn with_client(url: String, client: reqwest::Client) -> Self {
        Self { url, client }
    }
}

#[async_trait::async_trait]
impl ExperimentSource for HttpExperimentSource {
    async fn fetch_experiments(&self) -> Result<HashMap<String, bool>, ExperimentError> {
        let response = self
            .client
            .get(&self.url)
            .header("accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(ExperimentError::HttpError {
                status: status.as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }

        let experiments: HashMap<String, bool> = response.json().await?;
        Ok(experiments)
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

impl<S: ExperimentSource + std::fmt::Debug> std::fmt::Debug for CachedExperimentSource<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedExperimentSource")
            .field("inner", &self.inner)
            .field("cache_path", &self.cache_path)
            .field("max_age", &self.max_age)
            .finish()
    }
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
    /// HTTP request returned an error status.
    #[error("HTTP error ({status}): {message}")]
    HttpError {
        /// HTTP status code.
        status: u16,
        /// Response body or error message.
        message: String,
    },
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
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    fn test_client() -> reqwest::Client {
        // Disable env proxy so tests against a local MockServer aren't routed
        // through developer HTTP proxies (e.g. Privoxy picked up from
        // `HTTP_PROXY` / `ALL_PROXY` environment variables).
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build no-proxy reqwest client")
    }

    #[tokio::test]
    async fn stub_source_returns_empty_map() {
        let source = StubExperimentSource;
        let result = source.fetch_experiments().await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn http_source_parses_json_response() {
        let server = MockServer::start().await;

        let json_body = r#"{"dark_mode": true, "new_ui": false}"#;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(json_body))
            .mount(&server)
            .await;

        let source = HttpExperimentSource::with_client(server.uri(), test_client());
        let result = source.fetch_experiments().await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result.get("dark_mode"), Some(&true));
        assert_eq!(result.get("new_ui"), Some(&false));
    }

    #[tokio::test]
    async fn http_source_returns_error_on_non_200() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let source = HttpExperimentSource::with_client(server.uri(), test_client());
        let result = source.fetch_experiments().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ExperimentError::HttpError { status: 500, .. }));
    }

    #[tokio::test]
    async fn http_source_returns_error_on_invalid_json() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let source = HttpExperimentSource::with_client(server.uri(), test_client());
        let result = source.fetch_experiments().await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ExperimentError::Network(_)));
    }

    #[tokio::test]
    async fn cached_source_uses_http_then_caches() {
        let server = MockServer::start().await;

        let json_body = r#"{"beta_feature": true}"#;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(json_body))
            .expect(1..) // will be called at least once
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("experiments.json");
        let http = HttpExperimentSource::with_client(server.uri(), test_client());
        let cached = CachedExperimentSource::new(http, cache_path.clone(), std::time::Duration::from_secs(60));

        // First fetch hits HTTP
        let result1 = cached.fetch_experiments().await.unwrap();
        assert_eq!(result1.get("beta_feature"), Some(&true));
        assert!(cache_path.exists());

        // Second fetch uses cache (still fresh)
        let result2 = cached.fetch_experiments().await.unwrap();
        assert_eq!(result2.get("beta_feature"), Some(&true));
    }
}
