//! Telemetry uploader — batch-upload events and crash reports to a remote endpoint.

use crate::{TelemetryError, TelemetryStore};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Configuration for telemetry upload.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Endpoint URL for telemetry event uploads.
    pub endpoint: String,
    /// Explicit crash endpoint. If `None`, derived from `endpoint`.
    pub crash_endpoint: Option<String>,
    /// API key for authentication.
    pub api_key: Option<String>,
    /// Maximum events per batch.
    pub batch_size: usize,
    /// Upload interval in seconds.
    pub upload_interval_secs: u64,
    /// Whether telemetry is enabled.
    pub enabled: bool,
    /// Fraction of events to record (0.0–1.0).
    pub sample_rate: f64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://telemetry.coderex.dev/v1/events".into(),
            crash_endpoint: None,
            api_key: None,
            batch_size: 100,
            upload_interval_secs: 300,
            enabled: true,
            sample_rate: 1.0,
        }
    }
}

impl TelemetryConfig {
    /// Return the crash endpoint, using the explicit override if set
    /// or deriving it from the events endpoint.
    pub fn resolve_crash_endpoint(&self) -> String {
        if let Some(ref explicit) = self.crash_endpoint {
            return explicit.clone();
        }
        format!("{}/crashes", self.endpoint.trim_end_matches("/events"))
    }
}

/// Simple circuit breaker for upload failures.
#[derive(Debug)]
struct CircuitBreaker {
    /// 0 = Closed, 1 = Open, 2 = HalfOpen
    state: AtomicUsize,
    consecutive_failures: AtomicUsize,
    last_failure_ms: AtomicU64,
}

impl CircuitBreaker {
    const CLOSED: usize = 0;
    const OPEN: usize = 1;
    const HALF_OPEN: usize = 2;
    const FAILURE_THRESHOLD: usize = 5;
    const COOLDOWN_SECS: u64 = 60;

    fn new() -> Self {
        Self {
            state: AtomicUsize::new(Self::CLOSED),
            consecutive_failures: AtomicUsize::new(0),
            last_failure_ms: AtomicU64::new(0),
        }
    }

    /// Whether the next upload attempt should proceed.
    fn allow(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            Self::CLOSED => true,
            Self::OPEN => {
                let last = self.last_failure_ms.load(Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if now.saturating_sub(last) >= Self::COOLDOWN_SECS * 1000 {
                    self.state.store(Self::HALF_OPEN, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }
            Self::HALF_OPEN => true,
            _ => true,
        }
    }

    /// Call after a successful upload.
    fn on_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.state.store(Self::CLOSED, Ordering::Relaxed);
    }

    /// Call after a failed upload.
    fn on_failure(&self) {
        let count = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_failure_ms.store(now, Ordering::Relaxed);

        if count >= Self::FAILURE_THRESHOLD {
            self.state.store(Self::OPEN, Ordering::Relaxed);
        }
    }
}

/// Background uploader that periodically sends pending telemetry.
#[derive(Debug)]
pub struct TelemetryUploader {
    db_path: PathBuf,
    config: TelemetryConfig,
    client: reqwest::Client,
    circuit: CircuitBreaker,
}

impl Clone for TelemetryUploader {
    fn clone(&self) -> Self {
        Self {
            db_path: self.db_path.clone(),
            config: self.config.clone(),
            client: self.client.clone(),
            circuit: CircuitBreaker::new(),
        }
    }
}

impl TelemetryUploader {
    /// Create a new uploader.
    pub fn new(db_path: PathBuf, config: TelemetryConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self {
            db_path,
            config,
            client,
            circuit: CircuitBreaker::new(),
        }
    }

    /// Open a fresh connection to the telemetry database.
    fn open_store(&self) -> Result<TelemetryStore, TelemetryError> {
        TelemetryStore::open(&self.db_path)
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

        let mut interval =
            tokio::time::interval(Duration::from_secs(self.config.upload_interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if !self.circuit.allow() {
                tracing::debug!("telemetry upload circuit breaker open — skipping");
                continue;
            }

            match self.upload_pending().await {
                Ok(_) => self.circuit.on_success(),
                Err(e) => {
                    tracing::debug!("telemetry upload failed (will retry): {e}");
                    self.circuit.on_failure();
                }
            }
        }
    }

    /// Upload a single batch of pending events.
    pub async fn upload_pending(&self) -> Result<(), TelemetryError> {
        let store = self.open_store()?;

        let events = store.pending_events(self.config.batch_size)?;
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

        let body = compress_json(&payload)?;

        let mut request = self
            .client
            .post(&self.config.endpoint)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(body);

        if let Some(ref key) = self.config.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }

        match request.send().await {
            Ok(response) => {
                if response.status().is_success() {
                    store.mark_uploaded(&ids)?;
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
        let store = self.open_store()?;

        let crashes = store.pending_crashes(self.config.batch_size)?;
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

        let body = compress_json(&payload)?;
        let crash_endpoint = self.config.resolve_crash_endpoint();

        let mut request = self
            .client
            .post(&crash_endpoint)
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(body);

        if let Some(ref key) = self.config.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }

        match request.send().await {
            Ok(response) => {
                if response.status().is_success() {
                    store.mark_crashes_uploaded(&ids)?;
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

/// Compress a JSON value into a gzip-encoded byte vector.
fn compress_json(value: &serde_json::Value) -> Result<Vec<u8>, TelemetryError> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let json = serde_json::to_vec(value)?;
    // Skip compression for tiny payloads (< 1 KB)
    if json.len() < 1024 {
        return Ok(json);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json)?;
    encoder.finish().map_err(|e| TelemetryError::Io(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_config_defaults() {
        let config = TelemetryConfig::default();
        assert!(config.enabled);
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.upload_interval_secs, 300);
        assert_eq!(config.sample_rate, 1.0);
    }

    #[test]
    fn crash_endpoint_explicit() {
        let config = TelemetryConfig {
            crash_endpoint: Some("https://example.com/crashes".into()),
            ..Default::default()
        };
        assert_eq!(config.resolve_crash_endpoint(), "https://example.com/crashes");
    }

    #[test]
    fn crash_endpoint_derived() {
        let config = TelemetryConfig::default();
        assert_eq!(
            config.resolve_crash_endpoint(),
            "https://telemetry.coderex.dev/v1/crashes"
        );
    }

    #[test]
    fn circuit_breaker_allows_after_success() {
        let cb = CircuitBreaker::new();
        assert!(cb.allow());
        cb.on_success();
        assert!(cb.allow());
    }

    #[test]
    fn circuit_breaker_opens_after_failures() {
        let cb = CircuitBreaker::new();
        for _ in 0..CircuitBreaker::FAILURE_THRESHOLD {
            cb.on_failure();
        }
        // Circuit is open — allow() should return false because
        // the 60-second cooldown has not elapsed.
        assert!(!cb.allow());
    }

    #[test]
    fn compress_json_smoke() {
        let val = serde_json::json!({"key": "value"});
        let compressed = compress_json(&val).unwrap();
        // Small payload should not be compressed
        assert!(compressed.len() > 0);
    }
}
