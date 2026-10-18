//! Telemetry uploader — batch-upload events and crash reports to a remote endpoint.

use crate::{PendingCrash, PendingEvent, Scrubber, TelemetryError, TelemetryStore};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Optional pinned root certificate PEM for additional TLS trust.
    pub pinned_cert_pem: Option<String>,
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
            pinned_cert_pem: None,
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

/// Sliding-window rate limiter with a post-rejection suppression period.
///
/// The circuit breaker handles *failures* (5 in a row → open for 60s). This is
/// the complementary policy for *rejections*: a polite client sends at most
/// [`RateLimiter::MAX_PER_WINDOW`] uploads per [`RateLimiter::WINDOW`], and
/// after the endpoint answers with 429/403 it stops entirely for
/// [`RateLimiter::SUPPRESSION`] rather than retrying into the limit.
#[derive(Debug)]
struct RateLimiter {
    /// Timestamps of recent upload attempts, oldest first.
    window: Mutex<VecDeque<Instant>>,
    /// If set, uploads are suppressed until this instant.
    suppressed_until: Mutex<Option<Instant>>,
    max_per_window: usize,
    window_len: Duration,
    suppression: Duration,
}

impl RateLimiter {
    /// Maximum upload attempts allowed per window.
    const MAX_PER_WINDOW: usize = 10;
    /// Length of the sliding window.
    const WINDOW: Duration = Duration::from_secs(60);
    /// How long to stay quiet after the endpoint rejects an upload.
    const SUPPRESSION: Duration = Duration::from_secs(300);

    const fn new() -> Self {
        Self::with_limits(Self::MAX_PER_WINDOW, Self::WINDOW, Self::SUPPRESSION)
    }

    /// Construct a limiter with explicit limits (used by tests).
    const fn with_limits(
        max_per_window: usize,
        window_len: Duration,
        suppression: Duration,
    ) -> Self {
        Self {
            window: Mutex::new(VecDeque::new()),
            suppressed_until: Mutex::new(None),
            max_per_window,
            window_len,
            suppression,
        }
    }

    /// Whether an upload may proceed right now.
    fn allow(&self) -> bool {
        let now = Instant::now();

        if let Ok(suppressed) = self.suppressed_until.lock() {
            if let Some(until) = *suppressed {
                if now < until {
                    return false;
                }
            }
        }

        let Ok(mut window) = self.window.lock() else {
            return true;
        };
        while let Some(front) = window.front() {
            if now.duration_since(*front) >= self.window_len {
                window.pop_front();
            } else {
                break;
            }
        }
        window.len() < self.max_per_window
    }

    /// Record that an upload attempt is being made.
    fn record(&self) {
        if let Ok(mut window) = self.window.lock() {
            window.push_back(Instant::now());
        }
    }

    /// Enter the suppression period after the endpoint rejected an upload.
    fn on_rejection(&self) {
        if let Ok(mut suppressed) = self.suppressed_until.lock() {
            *suppressed = Some(Instant::now() + self.suppression);
        }
    }
}

/// Environment variable that turns on strict (confidentiality) redaction.
///
/// Present with any value other than `0`/`false`/`no`/empty enables the mode,
/// mirroring `CODEREX_DISABLE_ADOPT`.
pub const HIPAA_ENV: &str = "CODEREX_HIPAA";

/// Whether strict (confidentiality) redaction is requested for this process.
///
/// This is CodeRex's port of the reference's `hipaa` trigger. The reference
/// reads both an environment variable and a settings lookup; CodeRex's settings
/// layer is not reachable from this crate, so the environment variable is the
/// whole trigger. See `docs/telemetry.md`.
#[must_use]
pub fn hipaa_enabled() -> bool {
    hipaa_enabled_with(std::env::var(HIPAA_ENV).ok().as_deref())
}

/// Pure helper behind [`hipaa_enabled`]; testable without touching the process
/// environment.
#[must_use]
pub fn hipaa_enabled_with(value: Option<&str>) -> bool {
    value.is_some_and(|raw| {
        let trimmed = raw.trim();
        !(trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("0")
            || trimmed.eq_ignore_ascii_case("false")
            || trimmed.eq_ignore_ascii_case("no"))
    })
}

/// Background uploader that periodically sends pending telemetry.
#[derive(Debug)]
pub struct TelemetryUploader {
    db_path: PathBuf,
    config: TelemetryConfig,
    client: reqwest::Client,
    circuit: CircuitBreaker,
    rate: RateLimiter,
    scrubber: Scrubber,
    /// Resolved once at construction from [`hipaa_enabled`].
    strict: bool,
    /// Resolved once at construction from [`crate::crash_verbose`]. Ignored
    /// when `strict` is set: strict mode always wins.
    verbose: bool,
}

impl Clone for TelemetryUploader {
    fn clone(&self) -> Self {
        Self {
            db_path: self.db_path.clone(),
            config: self.config.clone(),
            client: self.client.clone(),
            circuit: CircuitBreaker::new(),
            rate: RateLimiter::new(),
            scrubber: self.scrubber.clone(),
            strict: self.strict,
            verbose: self.verbose,
        }
    }
}

impl TelemetryUploader {
    /// Create a new uploader.
    pub fn new(db_path: PathBuf, config: TelemetryConfig) -> Self {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));

        if let Some(ref pem) = config.pinned_cert_pem {
            match reqwest::Certificate::from_pem(pem.as_bytes()) {
                Ok(cert) => {
                    builder = builder.add_root_certificate(cert);
                    tracing::debug!("telemetry uploader: added pinned root certificate");
                }
                Err(e) => {
                    tracing::warn!("telemetry uploader: failed to parse pinned cert PEM: {e}");
                }
            }
        }

        let client = builder.build().unwrap_or_default();

        Self {
            db_path,
            config,
            client,
            circuit: CircuitBreaker::new(),
            rate: RateLimiter::new(),
            scrubber: Scrubber::new(),
            strict: hipaa_enabled(),
            verbose: crate::crash_verbose(),
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

            if !self.rate.allow() {
                tracing::debug!("telemetry upload rate limited or suppressed — skipping");
                continue;
            }
            self.rate.record();

            match self.upload_pending().await {
                Ok(_) => self.circuit.on_success(),
                Err(TelemetryError::Rejected { status }) => {
                    // The endpoint told us to back off. Suppress uploads for the
                    // suppression window instead of retrying into the limit.
                    tracing::debug!(
                        "telemetry upload rejected (HTTP {status}) — suppressing uploads for {}s",
                        RateLimiter::SUPPRESSION.as_secs()
                    );
                    self.rate.on_rejection();
                }
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
            "events": events
                .iter()
                .map(|e| build_event_payload(&self.scrubber, e, self.strict))
                .collect::<Vec<_>>(),
        });

        let body = encode_json(&payload)?;

        let mut request = body.apply(
            self.client
                .post(&self.config.endpoint)
                .header("content-type", "application/json"),
        );

        if let Some(ref key) = self.config.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }

        match request.send().await {
            Ok(response) => {
                if response.status().is_success() {
                    store.mark_uploaded(&ids)?;
                    tracing::debug!("uploaded {} telemetry events", ids.len());
                } else if is_rejection(response.status()) {
                    return Err(TelemetryError::Rejected {
                        status: response.status().as_u16(),
                    });
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
            "crashes": crashes
                .iter()
                .map(|c| build_crash_payload(&self.scrubber, c, self.verbose, self.strict))
                .collect::<Vec<_>>(),
        });

        let body = encode_json(&payload)?;
        let crash_endpoint = self.config.resolve_crash_endpoint();

        let mut request = body.apply(
            self.client
                .post(&crash_endpoint)
                .header("content-type", "application/json"),
        );

        if let Some(ref key) = self.config.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }

        match request.send().await {
            Ok(response) => {
                if response.status().is_success() {
                    store.mark_crashes_uploaded(&ids)?;
                    tracing::debug!("uploaded {} crash reports", ids.len());
                } else if is_rejection(response.status()) {
                    return Err(TelemetryError::Rejected {
                        status: response.status().as_u16(),
                    });
                } else {
                    return Err(TelemetryError::Upload(format!(
                        "HTTP {} (crashes)",
                        response.status()
                    )));
                }
            }
            Err(e) => {
                return Err(TelemetryError::Upload(format!("crash upload failed: {e}")));
            }
        }

        Ok(())
    }
}

/// Whether an HTTP status is an explicit "back off" signal from the endpoint.
fn is_rejection(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status == reqwest::StatusCode::FORBIDDEN
}

/// Build the wire payload for one pending event.
///
/// The stored payload is re-scrubbed on the way out (defence in depth: a
/// payload that reached the store unscrubbed still cannot carry a secret off
/// the machine). Under strict mode it must also validate against the field
/// allowlist — if it cannot, it is sent marked unredacted rather than dropped.
fn build_event_payload(
    scrubber: &Scrubber,
    event: &PendingEvent,
    strict: bool,
) -> serde_json::Value {
    let mut event_payload = serde_json::from_str::<serde_json::Value>(&event.payload)
        .unwrap_or(serde_json::Value::Null);
    scrubber.scrub_json(&mut event_payload);
    if strict {
        crate::scrubber::mark_unredacted_if_unallowlisted(&mut event_payload);
    }
    serde_json::json!({
        "type": event.event_type,
        "payload": event_payload,
        "timestamp": event.created_at,
    })
}

/// Build the wire payload for one pending crash report.
///
/// The default (and strict mode, which always wins) sends **metadata only**:
/// `panic_type`, `thread_name`, `panic_location`, the OS and CodeRex version,
/// and `redaction_status: "structured"`. No free-form panic text and no
/// backtrace leave the machine. A legacy raw row is reduced to its panic
/// location; its message is never forwarded.
///
/// Only `CODEREX_CRASH_VERBOSE` (without strict mode) sends the panic message
/// and backtrace. Both are re-scrubbed on the way out (defence in depth) and
/// the payload is marked `redaction_status: "pattern"` — never "clean".
fn build_crash_payload(
    scrubber: &Scrubber,
    crash: &PendingCrash,
    verbose: bool,
    strict: bool,
) -> serde_json::Value {
    let metadata = crate::crash::crash_metadata_from_stored(&crash.panic_info);
    let mut payload = serde_json::json!({
        "panic_type": metadata.panic_type,
        "thread_name": metadata.thread_name,
        "panic_location": metadata.panic_location,
        "os": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": crash.created_at,
    });

    // Strict mode wins over verbose. A row already ingested in structured form
    // has no free-form text to send, even under verbose.
    if strict || !verbose || crate::crash::is_structured(&crash.panic_info) {
        payload["redaction_status"] =
            serde_json::Value::String(crate::crash::CRASH_STRUCTURED_MARKER.to_string());
        if strict {
            // `redaction_status` and every structured field are on the
            // allowlist, so this leaves the explicit mark intact and only
            // marks genuinely unknown fields.
            crate::scrubber::mark_unredacted_if_unallowlisted(&mut payload);
        }
        return payload;
    }

    payload["panic_info"] = serde_json::Value::String(scrubber.scrub(&crash.panic_info));
    payload["backtrace"] = crash
        .backtrace
        .as_deref()
        .map(|backtrace| serde_json::Value::String(scrubber.scrub(backtrace)))
        .unwrap_or(serde_json::Value::Null);
    payload["redaction_status"] =
        serde_json::Value::String(crate::crash::CRASH_PATTERN_MARKER.to_string());
    payload
}

/// Compress a JSON value into a gzip-encoded byte vector.
/// A serialized request body and whether it was gzip-encoded.
///
/// The encoder skips compression for payloads too small to benefit, so the
/// caller cannot assume the bytes are gzipped — sending an uncompressed body
/// under `content-encoding: gzip` makes the receiver fail to decode it.
struct EncodedBody {
    bytes: Vec<u8>,
    gzipped: bool,
}

impl EncodedBody {
    /// Apply the body and the matching `content-encoding` header, if any.
    fn apply(self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.gzipped {
            request.header("content-encoding", "gzip").body(self.bytes)
        } else {
            request.body(self.bytes)
        }
    }
}

fn encode_json(value: &serde_json::Value) -> Result<EncodedBody, TelemetryError> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    const COMPRESSION_FLOOR_BYTES: usize = 1024;

    let json = serde_json::to_vec(value)?;
    if json.len() < COMPRESSION_FLOOR_BYTES {
        return Ok(EncodedBody {
            bytes: json,
            gzipped: false,
        });
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json)?;
    Ok(EncodedBody {
        bytes: encoder.finish().map_err(|e| TelemetryError::Io(e.into()))?,
        gzipped: true,
    })
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
        assert_eq!(
            config.resolve_crash_endpoint(),
            "https://example.com/crashes"
        );
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
    fn small_payload_is_sent_uncompressed() {
        let val = serde_json::json!({"key": "value"});
        let body = encode_json(&val).unwrap();
        assert!(
            !body.gzipped,
            "a payload below the compression floor must not claim gzip"
        );
        // Still valid JSON on the wire — the receiver reads it as-is.
        let round_trip: serde_json::Value =
            serde_json::from_slice(&body.bytes).expect("uncompressed body is plain JSON");
        assert_eq!(round_trip, val);
    }

    /// The bug this guards: the header was set unconditionally while the
    /// encoder skipped compression for small payloads, so the receiver was told
    /// to gunzip plain JSON. `apply` is the only place a body reaches the wire,
    /// and it decides the header from the same value that produced the bytes.
    #[test]
    fn content_encoding_header_matches_the_body() {
        let client = reqwest::Client::new();

        let small = encode_json(&serde_json::json!({"a": 1})).unwrap();
        assert!(!small.gzipped);
        let req = small
            .apply(client.post("http://localhost/"))
            .build()
            .unwrap();
        assert!(
            req.headers().get("content-encoding").is_none(),
            "uncompressed body must not send content-encoding: gzip"
        );

        let large = encode_json(&serde_json::json!({ "e": vec!["y".repeat(64); 64] })).unwrap();
        assert!(large.gzipped);
        let req = large
            .apply(client.post("http://localhost/"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "compressed body must declare gzip"
        );
    }

    #[test]
    fn large_payload_is_gzipped_and_decodes() {
        let val = serde_json::json!({ "events": vec!["x".repeat(64); 64] });
        let body = encode_json(&val).unwrap();
        assert!(body.gzipped, "a large payload should be compressed");

        // The header the sender sets must match what the bytes actually are.
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoded = Vec::new();
        GzDecoder::new(&body.bytes[..])
            .read_to_end(&mut decoded)
            .expect("body must be valid gzip when flagged as such");
        let round_trip: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(round_trip, val);
    }

    // ── Rate limiting / suppression ──────────────────────────────────────

    #[test]
    fn rate_limiter_allows_fresh_upload() {
        let limiter = RateLimiter::new();
        assert!(limiter.allow());
    }

    #[test]
    fn rate_limiter_blocks_after_max_in_window() {
        // Use explicit limits so this pins the *behavior* independently of the
        // production constants (which `rate_limiter_policy_matches_reference`
        // pins separately).
        let limiter =
            RateLimiter::with_limits(3, Duration::from_secs(60), Duration::from_secs(300));
        for i in 0..3 {
            assert!(limiter.allow(), "attempt {i} under the cap must be allowed");
            limiter.record();
        }
        assert!(
            !limiter.allow(),
            "the 4th attempt in the window must be refused"
        );
    }

    #[test]
    fn rate_limiter_suppresses_after_rejection() {
        let limiter = RateLimiter::new();
        assert!(limiter.allow());
        limiter.on_rejection();
        assert!(
            !limiter.allow(),
            "a rejection must suppress further uploads immediately"
        );
    }

    /// The suppression window must actually expire rather than being permanent.
    #[test]
    fn rate_limiter_suppression_expires() {
        let limiter =
            RateLimiter::with_limits(10, Duration::from_secs(60), Duration::from_millis(20));
        limiter.on_rejection();
        assert!(!limiter.allow());
        std::thread::sleep(Duration::from_millis(40));
        assert!(limiter.allow(), "uploads resume once the suppression ends");
    }

    /// The window must slide: attempts older than the window no longer count.
    #[test]
    fn rate_limiter_window_slides() {
        let limiter =
            RateLimiter::with_limits(2, Duration::from_millis(20), Duration::from_secs(300));
        limiter.record();
        limiter.record();
        assert!(!limiter.allow());
        std::thread::sleep(Duration::from_millis(40));
        assert!(limiter.allow(), "old attempts fall out of the window");
    }

    #[test]
    fn rate_limiter_policy_matches_reference() {
        assert_eq!(RateLimiter::MAX_PER_WINDOW, 10);
        assert_eq!(RateLimiter::WINDOW, Duration::from_secs(60));
        assert_eq!(RateLimiter::SUPPRESSION, Duration::from_secs(300));
    }

    #[test]
    fn only_back_off_statuses_are_rejections() {
        assert!(is_rejection(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_rejection(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_rejection(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_rejection(reqwest::StatusCode::BAD_REQUEST));
    }

    // ── Strict (HIPAA) trigger and payload marking ───────────────────────

    #[test]
    fn hipaa_env_values_parse() {
        assert!(hipaa_enabled_with(Some("1")));
        assert!(hipaa_enabled_with(Some("true")));
        assert!(hipaa_enabled_with(Some("YES")));
        assert!(!hipaa_enabled_with(Some("0")));
        assert!(!hipaa_enabled_with(Some("false")));
        assert!(!hipaa_enabled_with(Some("no")));
        assert!(!hipaa_enabled_with(Some("")));
        assert!(!hipaa_enabled_with(None));
    }

    #[test]
    fn hipaa_env_name_matches_the_documented_trigger() {
        assert_eq!(HIPAA_ENV, "CODEREX_HIPAA");
    }

    fn pending_event_with(payload: serde_json::Value) -> crate::PendingEvent {
        crate::PendingEvent {
            id: 1,
            event_type: "error".into(),
            payload: payload.to_string(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    /// A payload built from allowlisted fields is sent unmarked, even in strict
    /// mode.
    #[test]
    fn strict_mode_sends_allowlisted_event_unmarked() {
        let scrubber = Scrubber::new();
        let event = pending_event_with(serde_json::json!({
            "type": "error",
            "error_type": "io",
            "message": "boom",
            "timestamp": 1
        }));
        let built = build_event_payload(&scrubber, &event, true);
        let inner = &built["payload"];
        assert!(inner.get("redaction_status").is_none());
    }

    /// A payload the allowlist cannot validate is sent *marked* unredacted —
    /// the marker is in the payload, not a log line.
    #[test]
    fn strict_mode_marks_unallowlisted_event_unredacted() {
        let scrubber = Scrubber::new();
        let event = pending_event_with(serde_json::json!({
            "type": "error",
            "message": "boom",
            "conversation_snippet": "unexpected field"
        }));
        let built = build_event_payload(&scrubber, &event, true);
        assert_eq!(
            built["payload"]["redaction_status"],
            serde_json::json!("unredacted")
        );
    }

    /// Outside strict mode nothing is marked, even for an unknown field.
    #[test]
    fn non_strict_mode_does_not_mark() {
        let scrubber = Scrubber::new();
        let event = pending_event_with(serde_json::json!({
            "type": "error",
            "message": "boom",
            "conversation_snippet": "unexpected field"
        }));
        let built = build_event_payload(&scrubber, &event, false);
        assert!(built["payload"].get("redaction_status").is_none());
    }

    // ── Crash payloads ──────────────────────────────────────────────────

    fn pending_crash_with(panic_info: &str, backtrace: Option<&str>) -> PendingCrash {
        PendingCrash {
            id: 1,
            panic_info: panic_info.into(),
            backtrace: backtrace.map(str::to_string),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn assert_structured_payload(payload: &serde_json::Value) {
        assert_eq!(
            payload["redaction_status"],
            serde_json::json!("structured"),
            "the marker must be a payload field"
        );
        assert!(payload.get("panic_info").is_none(), "{payload}");
        assert!(payload.get("backtrace").is_none(), "{payload}");
        for field in [
            "panic_type",
            "thread_name",
            "panic_location",
            "os",
            "version",
        ] {
            assert!(
                payload.get(field).is_some_and(serde_json::Value::is_string),
                "{field} missing from {payload}"
            );
        }
        assert!(!payload["os"].as_str().unwrap().is_empty());
        assert!(!payload["version"].as_str().unwrap().is_empty());
    }

    /// The default sends metadata only: no free-form panic text and no
    /// backtrace, even for a legacy raw row.
    #[test]
    fn default_crash_payload_is_structured_and_marked() {
        let scrubber = Scrubber::new();
        let crash = pending_crash_with(
            "panicked at src/main.rs:10:5:\nuser said: key A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6 mail bob@example.com",
            Some("at /home/bob/project/src/main.rs:10:5"),
        );
        let payload = build_crash_payload(&scrubber, &crash, false, false);

        assert_structured_payload(&payload);
        assert_eq!(
            payload["panic_location"],
            serde_json::json!("src/main.rs:10:5")
        );
        assert_eq!(payload["panic_type"], serde_json::json!("unknown"));
        assert_eq!(payload["thread_name"], serde_json::json!("unknown"));

        let rendered = payload.to_string();
        for forbidden in [
            "user said",
            "bob@example.com",
            "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            "/home/bob",
        ] {
            assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
        }
    }

    /// A row ingested in structured form carries its metadata into the payload.
    #[test]
    fn structured_row_carries_panic_metadata() {
        let scrubber = Scrubber::new();
        let crash = pending_crash_with(
            "[strict-crash-summary] type=&str; thread=main; location=src/main.rs:10:5; message=omitted; backtrace=omitted",
            None,
        );
        let payload = build_crash_payload(&scrubber, &crash, false, false);
        assert_structured_payload(&payload);
        assert_eq!(payload["panic_type"], serde_json::json!("&str"));
        assert_eq!(payload["thread_name"], serde_json::json!("main"));
        assert_eq!(
            payload["panic_location"],
            serde_json::json!("src/main.rs:10:5")
        );
    }

    /// Strict mode sends structured metadata and no free-form text — and it
    /// wins even when verbose is enabled.
    #[test]
    fn strict_crash_payload_is_structured_and_wins_over_verbose() {
        let scrubber = Scrubber::new();
        let crash = pending_crash_with(
            "panicked at src/main.rs:10:5:\nuser said: key A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6 mail bob@example.com",
            Some("at /home/bob/project/src/main.rs:10:5"),
        );
        let payload = build_crash_payload(&scrubber, &crash, true, true);

        assert_structured_payload(&payload);
        let rendered = payload.to_string();
        for forbidden in [
            "user said",
            "bob@example.com",
            "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            "/home/bob",
        ] {
            assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
        }
        assert!(rendered.contains("src/main.rs:10:5"), "{rendered}");
    }

    /// A legacy raw row is reduced to its location under strict mode; it is
    /// never forwarded as if it had been scrubbed.
    #[test]
    fn strict_crash_payload_sanitizes_legacy_raw_rows() {
        let scrubber = Scrubber::new();
        let crash = pending_crash_with(
            "panicked at src/old.rs:3:1:\nuser said something private",
            Some("at /home/bob/secret.rs:3:1"),
        );
        let payload = build_crash_payload(&scrubber, &crash, false, true);
        let rendered = payload.to_string();
        assert!(!rendered.contains("user said"), "{rendered}");
        assert!(!rendered.contains("/home/bob"), "{rendered}");
        assert_eq!(
            payload["panic_location"],
            serde_json::json!("src/old.rs:3:1")
        );
        assert_structured_payload(&payload);
    }

    /// Verbose mode is the only path that sends free-form text, and it is
    /// scrubbed first and marked `pattern` — never claimed clean.
    #[test]
    fn verbose_crash_payload_is_pattern_scrubbed() {
        let scrubber = Scrubber::new();
        let crash = pending_crash_with(
            "panicked at src/main.rs:10:5:\nfailed for bob@example.com carrying A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            Some(
                "coderex_core::engine::run\n    at /home/bob/project/crates/core/src/engine.rs:42:9\nat file.rs:12:9",
            ),
        );
        let payload = build_crash_payload(&scrubber, &crash, true, false);

        assert_eq!(
            payload["redaction_status"],
            serde_json::json!("pattern"),
            "verbose payloads must not claim to be clean"
        );
        assert_eq!(
            payload["panic_location"],
            serde_json::json!("src/main.rs:10:5")
        );
        let rendered = payload.to_string();
        for forbidden in [
            "bob@example.com",
            "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            "/home/bob",
        ] {
            assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
        }
        for expected in [
            "failed for",
            "coderex_core::engine::run",
            "crates/core/src/engine.rs:42:9",
            "file.rs:12:9",
            "<SECRET>",
            "<EMAIL>",
        ] {
            assert!(rendered.contains(expected), "{expected}: {rendered}");
        }
    }

    /// A structured row has no free-form text to send; verbose mode therefore
    /// still reports it as structured rather than claiming a pattern scrub
    /// happened to text that does not exist.
    #[test]
    fn verbose_payload_for_a_structured_row_stays_structured() {
        let scrubber = Scrubber::new();
        let crash = pending_crash_with(
            "[strict-crash-summary] type=&str; thread=main; location=src/main.rs:1:1; message=omitted; backtrace=omitted",
            None,
        );
        let payload = build_crash_payload(&scrubber, &crash, true, false);
        assert_structured_payload(&payload);
        assert_eq!(payload["thread_name"], serde_json::json!("main"));
    }
}
