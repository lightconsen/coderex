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
mod scrubber;
mod store;
mod uploader;

pub use crash::{CRASH_VERBOSE_ENV, CrashReport, CrashReporter, crash_verbose, crash_verbose_with};
pub use scrubber::Scrubber;
pub use store::{PendingCrash, PendingEvent, TelemetryEvent, TelemetryStore};
pub use uploader::{
    HIPAA_ENV, TelemetryConfig, TelemetryUploader, hipaa_enabled, hipaa_enabled_with,
};

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::mpsc;
use thiserror::Error;

/// Command sent to the SQLite worker thread.
enum WorkerCommand {
    /// Record a telemetry event.
    Event(TelemetryEvent),
    /// Flush pending batch and signal completion.
    FlushAndExit(mpsc::Sender<()>),
}

/// Handle to the telemetry subsystem.
///
/// Cheaply cloneable and `Send`. Recorded events are forwarded to a
/// background worker that owns the SQLite connection.
#[derive(Debug, Clone)]
pub struct TelemetryHandle {
    sender: mpsc::Sender<WorkerCommand>,
    sample_rate: f64,
    scrubber: Option<Scrubber>,
}

impl TelemetryHandle {
    /// Initialize the telemetry subsystem.
    ///
    /// Spins up a dedicated SQLite worker thread and returns a handle
    /// plus an optional uploader task (if `config` is `Some` and `enabled`).
    pub fn new(
        store: TelemetryStore,
        config: Option<TelemetryConfig>,
    ) -> (Self, Option<TelemetryUploader>) {
        let (sender, receiver) = mpsc::channel::<WorkerCommand>();
        let db_path = store.path().to_path_buf();

        // Spawn the SQLite worker thread with panic recovery
        spawn_worker_with_recovery(receiver, db_path.clone());

        let sample_rate = config.as_ref().map(|c| c.sample_rate).unwrap_or(1.0);
        let handle = Self {
            sender,
            sample_rate,
            scrubber: Some(Scrubber::new()),
        };

        let uploader = config.and_then(|cfg| {
            if cfg.enabled {
                Some(TelemetryUploader::new(db_path, cfg))
            } else {
                None
            }
        });

        (handle, uploader)
    }

    /// Record a telemetry event.
    ///
    /// Non-blocking; events are buffered in a channel and flushed
    /// asynchronously by the worker thread.
    ///
    /// If sampling is configured and the event is skipped, this is a no-op.
    /// PII is scrubbed from `Error` and `ApiCall` events before storage.
    pub fn record(&self, mut event: TelemetryEvent) {
        if !should_sample(self.sample_rate, &event) {
            return;
        }
        if let Some(ref scrubber) = self.scrubber {
            scrubber.scrub_event(&mut event);
        }
        let _ = self.sender.send(WorkerCommand::Event(event));
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

    /// Signal the worker to flush its pending batch and exit cleanly.
    ///
    /// Blocks until the worker acknowledges completion or a 5-second
    /// timeout elapses.
    pub fn shutdown(&self) {
        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        let _ = self.sender.send(WorkerCommand::FlushAndExit(ack_tx));
        let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(5));
    }
}

/// Spawn the worker thread with panic recovery.
///
/// If the worker panics, it is respawned (up to 3 times) after a 10s cooldown.
/// A fresh SQLite connection is opened on each respawn to avoid poisoning.
fn spawn_worker_with_recovery(
    receiver: mpsc::Receiver<WorkerCommand>,
    db_path: std::path::PathBuf,
) {
    std::thread::spawn(move || {
        let mut respawn_count = 0;
        const MAX_RESPAWNS: usize = 3;
        const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);

        loop {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let store =
                    TelemetryStore::open(&db_path).expect("telemetry worker failed to open store");
                run_store_worker(&receiver, &store);
            }));

            match result {
                Ok(()) => break, // Clean exit (FlushAndExit or Disconnected)
                Err(_) => {
                    respawn_count += 1;
                    if respawn_count > MAX_RESPAWNS {
                        tracing::error!(
                            "telemetry worker panicked {respawn_count} times — giving up"
                        );
                        break;
                    }
                    tracing::warn!(
                        "telemetry worker panicked; respawning in {}s (attempt {respawn_count}/{MAX_RESPAWNS})",
                        COOLDOWN.as_secs()
                    );
                    std::thread::sleep(COOLDOWN);
                }
            }
        }
    });
}

/// Run the SQLite worker loop.
fn run_store_worker(receiver: &mpsc::Receiver<WorkerCommand>, store: &TelemetryStore) {
    let mut batch = Vec::with_capacity(32);
    let mut last_flush = std::time::Instant::now();
    const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const BATCH_SIZE: usize = 32;

    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(WorkerCommand::Event(event)) => {
                batch.push(event);
                if batch.len() >= BATCH_SIZE {
                    flush_batch(store, &mut batch);
                    last_flush = std::time::Instant::now();
                }
            }
            Ok(WorkerCommand::FlushAndExit(ack)) => {
                if !batch.is_empty() {
                    flush_batch(store, &mut batch);
                }
                let _ = ack.send(());
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !batch.is_empty() && last_flush.elapsed() >= FLUSH_INTERVAL {
                    flush_batch(store, &mut batch);
                    last_flush = std::time::Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed — flush remaining and exit
                if !batch.is_empty() {
                    flush_batch(store, &mut batch);
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

/// Determine whether an event should be recorded given a sample rate.
///
/// Uses a deterministic hash of the serialized event so the same event
/// always yields the same decision.
fn should_sample(sample_rate: f64, event: &TelemetryEvent) -> bool {
    if sample_rate >= 1.0 {
        return true;
    }
    if sample_rate <= 0.0 {
        return false;
    }
    let json = serde_json::to_string(event).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    json.hash(&mut hasher);
    let hash = hasher.finish();
    (hash as f64 / u64::MAX as f64) < sample_rate
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
    /// The endpoint explicitly rejected the upload (HTTP 429 or 403).
    ///
    /// Distinguished from a generic transport failure so the uploader can back
    /// off politely instead of retrying into a rate limit.
    #[error("upload rejected by server: HTTP {status}")]
    Rejected {
        /// The HTTP status code returned by the endpoint.
        status: u16,
    },
}
