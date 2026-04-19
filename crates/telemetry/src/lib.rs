//! Telemetry, crash reporting, and usage analytics for CodeRex.
//!
//! All telemetry is privacy-first and opt-out. No PII is collected.
//! Events are stored locally in SQLite and batch-uploaded when online.
//!
//! # Architecture
//!
//! - [`TelemetryHandle`] — `Clone` + `Send` handle for recording events from
//!   any thread or async task.
//! - [`TelemetryTask`] — background task that batches events and forwards
//!   them to a dedicated SQLite worker thread (because `rusqlite::Connection`
//!   is not `Send`).
//! - [`TelemetryUploader`] — async task that periodically uploads pending
//!   events to a remote endpoint.

#![warn(missing_docs)]

mod crash;
mod store;
mod uploader;

pub use crash::{CrashReport, CrashReporter};
pub use store::{PendingCrash, PendingEvent, TelemetryEvent, TelemetryStore};
pub use uploader::{TelemetryConfig, TelemetryUploader};

use std::sync::mpsc;
use thiserror::Error;

/// Handle to the telemetry subsystem.
///
/// Cheaply cloneable and `Send`. Recorded events are forwarded to a
/// background worker that owns the SQLite connection.
#[derive(Debug, Clone)]
pub struct TelemetryHandle {
    sender: mpsc::Sender<TelemetryEvent>,
}

impl TelemetryHandle {
    /// Initialize the telemetry subsystem.
    ///
    /// Spins up a dedicated SQLite worker thread and returns a handle
    /// plus an optional uploader task.
    pub fn new(
        store: TelemetryStore,
    ) -> (Self, Option<TelemetryUploader>) {
        let (sender, receiver) = mpsc::channel::<TelemetryEvent>();

        // Spawn the SQLite worker thread
        std::thread::spawn(move || {
            run_store_worker(receiver, store);
        });

        let handle = Self { sender };

        // Uploader is optional — created separately if upload is enabled
        (handle, None)
    }

    /// Record a telemetry event.
    ///
    /// Non-blocking; events are buffered in a channel and flushed
    /// asynchronously by the worker thread.
    pub fn record(&self, event: TelemetryEvent) {
        let _ = self.sender.send(event);
    }

    /// Record a session-start event.
    pub fn session_start(&self, session_id: String) {
        self.record(TelemetryEvent::SessionStart {
            session_id,
            timestamp: chrono::Utc::now(),
        });
    }

    /// Record a tool-usage event.
    pub fn tool_used(&self, tool_name: String, success: bool, duration_ms: u64) {
        self.record(TelemetryEvent::ToolUsed {
            tool_name,
            success,
            duration_ms,
            timestamp: chrono::Utc::now(),
        });
    }

    /// Record an API call event.
    pub fn api_call(&self, provider: String, model: String, tokens_in: u32, tokens_out: u32) {
        self.record(TelemetryEvent::ApiCall {
            provider,
            model,
            tokens_in,
            tokens_out,
            timestamp: chrono::Utc::now(),
        });
    }

    /// Record an error event.
    pub fn error(&self, error_type: String, message: String) {
        self.record(TelemetryEvent::Error {
            error_type,
            message,
            timestamp: chrono::Utc::now(),
        });
    }
}

/// Run the SQLite worker loop.
fn run_store_worker(receiver: mpsc::Receiver<TelemetryEvent>, store: TelemetryStore) {
    let mut batch = Vec::with_capacity(32);
    let mut last_flush = std::time::Instant::now();
    const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const BATCH_SIZE: usize = 32;

    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(event) => {
                batch.push(event);
                if batch.len() >= BATCH_SIZE {
                    flush_batch(&store, &mut batch);
                    last_flush = std::time::Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !batch.is_empty() && last_flush.elapsed() >= FLUSH_INTERVAL {
                    flush_batch(&store, &mut batch);
                    last_flush = std::time::Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed — flush remaining and exit
                if !batch.is_empty() {
                    flush_batch(&store, &mut batch);
                }
                break;
            }
        }
    }
}

fn flush_batch(store: &TelemetryStore, batch: &mut Vec<TelemetryEvent>) {
    if batch.is_empty() {
        return;
    }
    let to_flush: Vec<TelemetryEvent> = batch.drain(..).collect();
    if let Err(e) = store.insert_batch(&to_flush) {
        tracing::warn!("failed to flush telemetry batch: {e}");
    }
}

/// Errors from telemetry operations.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// Database error.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// HTTP upload error.
    #[error("upload error: {0}")]
    Upload(String),
}
