//! HTTP transport for team memory sync.
//!
//! Implements [`TeamSyncTransport`](crate::team_memory::TeamSyncTransport)
//! over HTTP/JSON for communicating with a relay server.

use async_trait::async_trait;
use reqwest::Client;

use crate::team_memory::{RemoteState, TeamMemoryEntry, TeamSyncTransport};

/// Errors from HTTP transport operations.
#[derive(Debug, thiserror::Error)]
pub enum HttpTransportError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

/// HTTP-based team memory transport.
///
/// Communicates with a relay server at `relay_url` using three endpoints:
/// - `GET {relay_url}/state` — fetch remote checksums
/// - `POST {relay_url}/delta` — send local changes
/// - `POST {relay_url}/entries` — fetch full entry content by topic
#[derive(Debug)]
pub struct HttpTransport {
    client: Client,
    relay_url: String,
}

impl HttpTransport {
    /// Create a new HTTP transport pointing at the relay URL.
    pub fn new(relay_url: String) -> Self {
        Self {
            client: Client::new(),
            relay_url: relay_url.trim_end_matches('/').to_string(),
        }
    }

    /// Create with a custom reqwest client (e.g. with timeout, auth headers).
    pub fn with_client(relay_url: String, client: Client) -> Self {
        Self {
            client,
            relay_url: relay_url.trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait]
impl TeamSyncTransport for HttpTransport {
    async fn fetch_remote_state(&self) -> Result<RemoteState, String> {
        let resp = self
            .client
            .get(format!("{}/state", self.relay_url))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("fetch state failed with status: {}", resp.status()));
        }

        resp.json::<RemoteState>().await.map_err(|e| e.to_string())
    }

    async fn send_delta(&self, entries: &[TeamMemoryEntry]) -> Result<(), String> {
        self.client
            .post(format!("{}/delta", self.relay_url))
            .json(entries)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn fetch_entries(&self, topics: &[String]) -> Result<Vec<TeamMemoryEntry>, String> {
        let resp = self
            .client
            .post(format!("{}/entries", self.relay_url))
            .json(topics)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!(
                "fetch entries failed with status: {}",
                resp.status()
            ));
        }

        resp.json::<Vec<TeamMemoryEntry>>()
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team_memory::MemoryChecksum;
    use chrono::Utc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_remote_state_returns_parsed_state() {
        let mock = MockServer::start().await;

        let cs = MemoryChecksum::from_content("test");
        let now = Utc::now();
        let state = RemoteState::default();
        let state_json = serde_json::to_value(&state).unwrap();

        Mock::given(method("GET"))
            .and(path("/state"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": {
                    "topic-a": [cs, false, now.to_rfc3339()]
                }
            })))
            .mount(&mock)
            .await;

        let transport = HttpTransport::new(mock.uri());
        let state = transport.fetch_remote_state().await.unwrap();
        assert!(state.entries.contains_key("topic-a"));
    }

    #[tokio::test]
    async fn send_delta_posts_entries() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/delta"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;

        let transport = HttpTransport::new(mock.uri());
        let entries = vec![TeamMemoryEntry {
            topic: "test".into(),
            content: "hello".into(),
            checksum: MemoryChecksum::from_content("hello"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tombstoned: false,
            version: 0,
        }];

        transport.send_delta(&entries).await.unwrap();
    }

    #[tokio::test]
    async fn fetch_entries_returns_entries() {
        let mock = MockServer::start().await;

        let entry = TeamMemoryEntry {
            topic: "fetched".into(),
            content: "from server".into(),
            checksum: MemoryChecksum::from_content("from server"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tombstoned: false,
            version: 0,
        };

        Mock::given(method("POST"))
            .and(path("/entries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([entry])))
            .mount(&mock)
            .await;

        let transport = HttpTransport::new(mock.uri());
        let entries = transport
            .fetch_entries(&["fetched".to_string()])
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].topic, "fetched");
    }

    #[tokio::test]
    async fn fetch_remote_state_error_on_failure() {
        let transport = HttpTransport::new("http://localhost:1".to_string());
        let result = transport.fetch_remote_state().await;
        assert!(result.is_err());
    }
}
