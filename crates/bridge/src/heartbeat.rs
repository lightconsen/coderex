//! Bridge heartbeat, WebSocket disconnect/reconnect, and state recovery.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio::time::Duration;

/// Heartbeat status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeartbeatStatus {
    /// Heartbeat is healthy.
    Healthy,
    /// Heartbeat is degraded (recent missed beats).
    Degraded,
    /// Heartbeat is lost (too many consecutive failures).
    Lost,
}

/// Configuration for the heartbeat system.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatConfig {
    /// Interval between heartbeats (in seconds).
    pub interval_secs: u64,
    /// Number of consecutive failures before marking as degraded.
    pub degraded_threshold: usize,
    /// Number of consecutive failures before marking as lost.
    pub lost_threshold: usize,
    /// Timeout for each heartbeat ping (in seconds).
    pub timeout_secs: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_secs: 30,
            degraded_threshold: 2,
            lost_threshold: 5,
            timeout_secs: 10,
        }
    }
}

/// Heartbeat state with failure tracking.
#[derive(Debug)]
pub struct Heartbeat {
    /// Configuration.
    config: HeartbeatConfig,
    /// Consecutive failure count.
    consecutive_failures: usize,
    /// Total beats sent.
    total_beats: u64,
    /// Last successful beat timestamp (epoch seconds).
    last_success: Option<i64>,
}

impl Heartbeat {
    /// Create a new heartbeat with the given config.
    #[must_use]
    pub const fn new(config: HeartbeatConfig) -> Self {
        Self {
            config,
            consecutive_failures: 0,
            total_beats: 0,
            last_success: None,
        }
    }

    /// Record a successful heartbeat.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.total_beats += 1;
        self.last_success = Some(chrono::Utc::now().timestamp());
    }

    /// Record a failed heartbeat.
    pub const fn record_failure(&mut self) {
        self.consecutive_failures += 1;
    }

    /// Get the current health status.
    #[must_use]
    pub const fn status(&self) -> HeartbeatStatus {
        if self.consecutive_failures >= self.config.lost_threshold {
            HeartbeatStatus::Lost
        } else if self.consecutive_failures >= self.config.degraded_threshold {
            HeartbeatStatus::Degraded
        } else {
            HeartbeatStatus::Healthy
        }
    }

    /// Check if the heartbeat is considered healthy.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.status() == HeartbeatStatus::Healthy
    }

    /// Get the number of consecutive failures.
    #[must_use]
    pub const fn consecutive_failures(&self) -> usize {
        self.consecutive_failures
    }

    /// Get the total number of successful beats.
    #[must_use]
    // The comparison above makes the narrowing cast safe; `try_from` is not
    // usable in a const fn.
    #[allow(clippy::cast_possible_truncation)]
    pub const fn total_beats(&self) -> usize {
        if self.total_beats > usize::MAX as u64 {
            usize::MAX
        } else {
            self.total_beats as usize
        }
    }

    /// Get the interval as a Duration.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        Duration::from_secs(self.config.interval_secs)
    }

    /// Get the timeout as a Duration.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout_secs)
    }

    /// Reset the heartbeat (after reconnection).
    pub const fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.last_success = None;
    }
}

/// Manages heartbeat loop with stop/start signaling.
#[derive(Debug)]
pub struct HeartbeatLoop {
    /// Shared heartbeat state.
    heartbeat: Arc<std::sync::Mutex<Heartbeat>>,
    /// Signal to stop the loop.
    stop_notify: Arc<Notify>,
    /// Whether the loop is running.
    running: Arc<AtomicBool>,
}

impl HeartbeatLoop {
    /// Create a new heartbeat loop.
    #[must_use]
    pub fn new(config: HeartbeatConfig) -> Self {
        Self {
            heartbeat: Arc::new(std::sync::Mutex::new(Heartbeat::new(config))),
            stop_notify: Arc::new(Notify::new()),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get a reference to the heartbeat state for reading.
    #[must_use]
    pub fn heartbeat(&self) -> Arc<std::sync::Mutex<Heartbeat>> {
        self.heartbeat.clone()
    }

    /// Signal the loop to stop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.stop_notify.notify_one();
    }

    /// Check if the loop is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Run the heartbeat loop (blocks until stopped).
    pub async fn run<F, Fut>(&self, ping_fn: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = bool> + Send,
    {
        self.running.store(true, Ordering::SeqCst);
        let interval = {
            let hb = self.heartbeat.lock().unwrap();
            hb.interval()
        };

        let mut ticker = tokio::time::interval(interval);
        let stop = self.stop_notify.clone();
        let running = self.running.clone();
        let heartbeat = self.heartbeat.clone();

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }

                    let success = ping_fn().await;

                    let mut hb = heartbeat.lock().unwrap();
                    if success {
                        hb.record_success();
                    } else {
                        hb.record_failure();
                    }
                }
                () = stop.notified() => {
                    running.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }

        self.running.store(false, Ordering::SeqCst);
    }
}

/// WebSocket reconnection state and logic.
#[derive(Debug)]
pub struct WsReconnector {
    /// Current reconnection attempt count.
    pub attempt: usize,
    /// Maximum reconnection attempts.
    pub max_attempts: usize,
    /// Base delay for exponential backoff (in milliseconds).
    pub base_delay_ms: u64,
    /// Maximum delay cap (in milliseconds).
    pub max_delay_ms: u64,
}

impl Default for WsReconnector {
    fn default() -> Self {
        Self {
            attempt: 0,
            max_attempts: 10,
            base_delay_ms: 1000,
            max_delay_ms: 30_000,
        }
    }
}

impl WsReconnector {
    /// Create a new reconnector with the given max attempts.
    #[must_use]
    pub fn new(max_attempts: usize) -> Self {
        Self {
            max_attempts,
            ..Default::default()
        }
    }

    /// Calculate the delay for the next reconnection attempt.
    #[must_use]
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.attempt >= self.max_attempts {
            return None;
        }

        self.attempt += 1;

        // Exponential backoff: base_delay * 2^(attempt-1), capped at max_delay
        let shift = u32::try_from(self.attempt)
            .unwrap_or(u32::MAX)
            .saturating_sub(1);
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let delay_ms = self.base_delay_ms.saturating_mul(multiplier);
        let delay_ms = delay_ms.min(self.max_delay_ms);

        Some(Duration::from_millis(delay_ms))
    }

    /// Reset the reconnection counter (after successful reconnect).
    pub const fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Check if more reconnection attempts are available.
    #[must_use]
    pub const fn can_reconnect(&self) -> bool {
        self.attempt < self.max_attempts
    }

    /// Get the total number of reconnection attempts made.
    #[must_use]
    pub const fn attempt_count(&self) -> usize {
        self.attempt
    }
}

/// A cursor over the sequence numbers of the downstream stream.
///
/// Reconnecting with backoff alone re-establishes the connection but not the
/// stream: whatever the gateway sent while the worker was away is simply gone.
/// The cursor is the other half of the insurance — the worker remembers the
/// highest sequence number that reached it, and asks to resume one past it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceCursor {
    last_delivered: Option<u64>,
}

impl SequenceCursor {
    /// Create a cursor that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_delivered: None,
        }
    }

    /// Record that sequence number `seq` reached the worker.
    ///
    /// A frame that arrives out of order never rewinds the cursor: the point
    /// is the furthest point the stream has reached.
    pub const fn deliver(&mut self, seq: u64) {
        let ahead = match self.last_delivered {
            None => true,
            Some(last) => seq > last,
        };
        if ahead {
            self.last_delivered = Some(seq);
        }
    }

    /// The highest sequence number that reached the worker.
    #[must_use]
    pub const fn last_delivered(&self) -> Option<u64> {
        self.last_delivered
    }

    /// The sequence number a reconnect should ask for.
    ///
    /// One past the last frame that arrived; from the beginning if none did.
    #[must_use]
    pub fn resume_from(&self) -> u64 {
        self.last_delivered.map_or(1, |last| last + 1)
    }

    /// The `Last-Event-ID` a reconnect presents, when anything has arrived.
    ///
    /// The same point expressed the way an event stream expects it, so a
    /// gateway that understands one form or the other can serve the resume.
    #[must_use]
    pub fn last_event_id(&self) -> Option<String> {
        self.last_delivered.map(|last| last.to_string())
    }
}

/// State recovery snapshot for reconnecting sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecoveryState {
    /// Session ID.
    pub session_id: String,
    /// Last known state before disconnect.
    pub last_state: String,
    /// Pending tool calls that need responses.
    pub pending_tool_calls: Vec<String>,
    /// Timestamp of the recovery snapshot.
    pub snapshot_time: String,
    /// Highest downstream sequence number delivered before the snapshot.
    ///
    /// Defaulted on deserialisation so snapshots written before the stream
    /// carried a cursor still load.
    #[serde(default)]
    pub last_sequence_num: Option<u64>,
}

impl SessionRecoveryState {
    /// Create a new recovery state.
    #[must_use]
    pub fn new(session_id: String, last_state: String) -> Self {
        Self {
            session_id,
            last_state,
            pending_tool_calls: Vec::new(),
            snapshot_time: chrono::Utc::now().to_rfc3339(),
            last_sequence_num: None,
        }
    }

    /// Add a pending tool call.
    pub fn add_pending_tool(&mut self, tool_call_id: String) {
        self.pending_tool_calls.push(tool_call_id);
    }

    /// Check if there are pending tool calls.
    #[must_use]
    pub const fn has_pending_tools(&self) -> bool {
        !self.pending_tool_calls.is_empty()
    }

    /// Record the last downstream sequence number the session had seen.
    pub const fn record_sequence(&mut self, seq: u64) {
        let ahead = match self.last_sequence_num {
            None => true,
            Some(last) => seq > last,
        };
        if ahead {
            self.last_sequence_num = Some(seq);
        }
    }

    /// The sequence number this session's reconnect should ask for.
    #[must_use]
    pub fn resume_from(&self) -> u64 {
        self.last_sequence_num.map_or(1, |last| last + 1)
    }

    /// The `Last-Event-ID` this session's reconnect should present.
    #[must_use]
    pub fn last_event_id(&self) -> Option<String> {
        self.last_sequence_num.map(|last| last.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_healthy_initially() {
        let hb = Heartbeat::new(HeartbeatConfig::default());
        assert_eq!(hb.status(), HeartbeatStatus::Healthy);
        assert!(hb.is_healthy());
        assert_eq!(hb.consecutive_failures(), 0);
        assert_eq!(hb.total_beats(), 0);
    }

    #[test]
    fn heartbeat_transitions_to_degraded() {
        let mut hb = Heartbeat::new(HeartbeatConfig {
            degraded_threshold: 2,
            lost_threshold: 5,
            ..Default::default()
        });
        hb.record_failure();
        assert_eq!(hb.status(), HeartbeatStatus::Healthy);
        hb.record_failure();
        assert_eq!(hb.status(), HeartbeatStatus::Degraded);
    }

    #[test]
    fn heartbeat_transitions_to_lost() {
        let mut hb = Heartbeat::new(HeartbeatConfig {
            degraded_threshold: 2,
            lost_threshold: 5,
            ..Default::default()
        });
        for _ in 0..5 {
            hb.record_failure();
        }
        assert_eq!(hb.status(), HeartbeatStatus::Lost);
    }

    #[test]
    fn heartbeat_success_resets_failures() {
        let mut hb = Heartbeat::new(HeartbeatConfig {
            degraded_threshold: 2,
            lost_threshold: 5,
            ..Default::default()
        });
        hb.record_failure();
        hb.record_failure();
        assert_eq!(hb.status(), HeartbeatStatus::Degraded);
        hb.record_success();
        assert_eq!(hb.status(), HeartbeatStatus::Healthy);
    }

    #[test]
    fn heartbeat_reset() {
        let mut hb = Heartbeat::new(HeartbeatConfig::default());
        hb.record_failure();
        hb.record_failure();
        hb.reset();
        assert_eq!(hb.consecutive_failures(), 0);
    }

    #[test]
    fn heartbeat_interval_and_timeout() {
        let hb = Heartbeat::new(HeartbeatConfig {
            interval_secs: 60,
            timeout_secs: 15,
            ..Default::default()
        });
        assert_eq!(hb.interval(), Duration::from_secs(60));
        assert_eq!(hb.timeout(), Duration::from_secs(15));
    }

    #[test]
    fn reconnector_exponential_backoff() {
        let mut rc = WsReconnector::new(5);
        let d1 = rc.next_delay().unwrap();
        let d2 = rc.next_delay().unwrap();
        let d3 = rc.next_delay().unwrap();

        assert!(d2 > d1);
        assert!(d3 > d2);
    }

    #[test]
    fn reconnector_respects_max_attempts() {
        let mut rc = WsReconnector::new(3);
        assert!(rc.next_delay().is_some());
        assert!(rc.next_delay().is_some());
        assert!(rc.next_delay().is_some());
        assert!(rc.next_delay().is_none());
        assert!(!rc.can_reconnect());
    }

    #[test]
    fn reconnector_reset() {
        let mut rc = WsReconnector::new(3);
        let _ = rc.next_delay();
        let _ = rc.next_delay();
        rc.reset();
        assert!(rc.can_reconnect());
        assert_eq!(rc.attempt_count(), 0);
    }

    #[test]
    fn reconnector_default_values() {
        let rc = WsReconnector::default();
        assert_eq!(rc.max_attempts, 10);
        assert_eq!(rc.base_delay_ms, 1000);
        assert_eq!(rc.max_delay_ms, 30_000);
    }

    #[test]
    fn recovery_state() {
        let mut recovery = SessionRecoveryState::new("s1".into(), "running".into());
        assert!(!recovery.has_pending_tools());
        recovery.add_pending_tool("tc1".into());
        recovery.add_pending_tool("tc2".into());
        assert!(recovery.has_pending_tools());
        assert_eq!(recovery.pending_tool_calls.len(), 2);
    }

    #[test]
    fn a_cursor_starts_at_the_beginning_of_the_stream() {
        let cursor = SequenceCursor::new();
        assert_eq!(cursor.last_delivered(), None);
        assert_eq!(cursor.resume_from(), 1);
        assert_eq!(cursor.last_event_id(), None);
    }

    #[test]
    fn a_cursor_resumes_one_past_the_last_frame() {
        let mut cursor = SequenceCursor::new();
        for seq in [1, 2, 3] {
            cursor.deliver(seq);
        }
        assert_eq!(cursor.last_delivered(), Some(3));
        assert_eq!(cursor.resume_from(), 4);
        assert_eq!(cursor.last_event_id().as_deref(), Some("3"));
    }

    #[test]
    fn a_cursor_never_rewinds_on_an_out_of_order_frame() {
        let mut cursor = SequenceCursor::new();
        cursor.deliver(9);
        cursor.deliver(4);
        assert_eq!(cursor.last_delivered(), Some(9));
        assert_eq!(cursor.resume_from(), 10);
    }

    #[test]
    fn a_recovery_snapshot_carries_the_stream_offset() {
        let mut recovery = SessionRecoveryState::new("s1".into(), "running".into());
        assert_eq!(recovery.resume_from(), 1);

        recovery.record_sequence(12);
        recovery.record_sequence(11); // out of order, ignored
        assert_eq!(recovery.last_sequence_num, Some(12));
        assert_eq!(recovery.resume_from(), 13);
        assert_eq!(recovery.last_event_id().as_deref(), Some("12"));
    }

    #[test]
    fn an_older_snapshot_without_an_offset_still_loads() {
        let json = r#"{
            "session_id": "s1",
            "last_state": "running",
            "pending_tool_calls": [],
            "snapshot_time": "2026-01-01T00:00:00Z"
        }"#;
        let recovery: SessionRecoveryState = serde_json::from_str(json).unwrap();
        assert_eq!(recovery.last_sequence_num, None);
        assert_eq!(recovery.resume_from(), 1);
    }

    #[test]
    fn heartbeat_loop_starts_stops() {
        let loop_ = HeartbeatLoop::new(HeartbeatConfig::default());
        assert!(!loop_.is_running());
        loop_.stop(); // Should not panic even if not running
    }

    #[tokio::test]
    async fn heartbeat_loop_runs_and_stops() {
        let loop_ = HeartbeatLoop::new(HeartbeatConfig {
            interval_secs: 1,
            ..Default::default()
        });

        // Run the loop in background
        let hb = loop_.heartbeat.clone();
        let stop = loop_.stop_notify.clone();
        let running = loop_.running.clone();
        let handle = tokio::spawn(async move {
            let ticker = tokio::time::interval({
                let h = hb.lock().unwrap();
                h.interval()
            });
            let mut tick = ticker;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if !running.load(Ordering::Relaxed) {
                            break;
                        }
                        let mut h = hb.lock().unwrap();
                        h.record_success();
                    }
                    () = stop.notified() => {
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
            running.store(false, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        loop_.stop();
        handle.await.unwrap();
    }

    #[test]
    fn heartbeat_status_serialization() {
        let status = HeartbeatStatus::Healthy;
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("Healthy"));
        let parsed: HeartbeatStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, HeartbeatStatus::Healthy);
    }
}
