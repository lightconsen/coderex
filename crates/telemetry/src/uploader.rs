//! Telemetry uploader — batch-upload events and crash reports to a remote endpoint.

use crate::{TelemetryError, TelemetryStore};
use std::sync::Arc;
use std::time::Duration;

/// Configuration for telemetry upload.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Endpoint URL for telemetry uploads.
    pub endpoint: String,
    /// API key for authentication.
    pub api_key: Option<String>,
    /// Maximum events per batch.
    pub batch_size: usize,
    /// Upload interval in seconds.
    pub upload_interval_secs: u64,
    /// Whether telemetry is enabled.
    pub enabled: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://telemetry.coderex.dev/v1/events".into(),
            api_key: None,
            batch_size: 100,
            upload_interval_secs: 300,
            enabled: true,
        }
    }
}

/// Background uploader that periodically sends pending telemetry.
#[derive(Debug, Clone)]
pub struct TelemetryUploader {
    store: Arc<TelemetryStore>,
    config: TelemetryConfig,
    client: reqwest::Client,
}

impl TelemetryUploader {
    /// Create a new uploader.
    pub fn new(store: Arc<TelemetryStore>, config: TelemetryConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self {
            store,
            config,
            client,
        }
    }

    /// Run the upload loop.
    ///
    /// Periodically checks for pending events and uploads them in batches.
    /// If the upload fails, events remain in the store for retry.
    pub async fn run(&self) {
        if !self.config.enabled {
            tracing::info!("telemetry upload disabled");
            return;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(self.config.upload_interval_secs));
        // Don't fire immediately on startup — wait one interval
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if let Err(e) = self.upload_pending().await {
                tracing::debug!("telemetry upload failed (will retry): {e}");
            }
        }
    }

    /// Upload a single batch of pending events.
    pub async fn upload_pending(&self) -> Result<(), TelemetryError> {
        let events = self.store.pending_events(self.config.batch_size)?;
        if events.is_empty() {
            return Ok(());
        }

        let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
        let payload = serde_json::json!({
            "events": events.iter().map(|e| {
                serde_json::json!({
                    "type": e.event_type,
                    "payload": serde_json::from_str::<serde_json::Value>(&e.payload).unwrap_or(serde_json::Value::Null),
                    "timestamp": e.created_at,
                })
            }).collect::<Vec<_>>(),
        });

        let mut request = self
            .client
            .post(&self.config.endpoint)
            .header("content-type", "application/json")
            .json(&payload);

        if let Some(ref key) = self.config.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }

        match request.send().await {
            Ok(response) => {
                if response.status().is_success() {
                    self.store.mark_uploaded(&ids)?;
                    tracing::debug!("uploaded {} telemetry events", ids.len());
                } else {
                    return Err(TelemetryError::Upload(format!(
                        "HTTP {}",
                        response.status()
                    )));
                }
            }
            Err(e) => {
                return Err(TelemetryError::Upload(format!("request failed: {e}")));
            }
        }

        // Also upload crash reports
        self.upload_pending_crashes().await?;

        Ok(())
    }

    async fn upload_pending_crashes(&self) -> Result<(), TelemetryError> {
        let crashes = self.store.pending_crashes(self.config.batch_size)?;
        if crashes.is_empty() {
            return Ok(());
        }

        let ids: Vec<i64> = crashes.iter().map(|c| c.id).collect();
        let payload = serde_json::json!({
            "crashes": crashes.iter().map(|c| {
                serde_json::json!({
                    "panic_info": c.panic_info,
                    "backtrace": c.backtrace,
                    "timestamp": c.created_at,
                })
            }).collect::<Vec<_>>(),
        });

        let crash_endpoint = format!("{}/crashes", self.config.endpoint.trim_end_matches("/events"));
        let mut request = self
            .client
            .post(&crash_endpoint)
            .header("content-type", "application/json")
            .json(&payload);

        if let Some(ref key) = self.config.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }

        match request.send().await {
            Ok(response) => {
                if response.status().is_success() {
                    self.store.mark_crashes_uploaded(&ids)?;
                    tracing::debug!("uploaded {} crash reports", ids.len());
                } else {
                    return Err(TelemetryError::Upload(format!(
                        "HTTP {} (crashes)",
                        response.status()
                    )));
                }
            }
            Err(e) => {
                return Err(TelemetryError::Upload(format!(
                    "crash upload failed: {e}"
                )));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TelemetryStore;

    #[test]
    fn telemetry_config_defaults() {
        let config = TelemetryConfig::default();
        assert!(config.enabled);
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.upload_interval_secs, 300);
    }
}
