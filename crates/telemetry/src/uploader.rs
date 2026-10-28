//! Telemetry uploader — batch-upload events and crash reports to a remote endpoint.

use crate::{
    ChannelConfig, PendingCrash, PendingEvent, Scrubber, TelemetryChannels, TelemetryError,
    TelemetryStore,
};
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
    /// Enable strict (HIPAA-style) redaction from configuration.
    ///
    /// The environment variable still wins: `strict` is on when either this
    /// is set or `CODEREX_HIPAA` says so. Configuration can only add the
    /// stricter mode, never remove it, because it is a tightening.
    pub strict: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            // No built-in destination: an enabled configuration must name an
            // endpoint, otherwise telemetry stays in the local database.
            endpoint: String::new(),
            crash_endpoint: None,
            api_key: None,
            batch_size: 100,
            upload_interval_secs: 300,
            enabled: false,
            sample_rate: 1.0,
            pinned_cert_pem: None,
            strict: false,
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
        if self.endpoint.trim().is_empty() {
            return String::new();
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

    const fn new() -> Self {
        Self {
            state: AtomicUsize::new(Self::CLOSED),
            consecutive_failures: AtomicUsize::new(0),
            last_failure_ms: AtomicU64::new(0),
        }
    }

    /// Whether the next upload attempt should proceed.
    fn allow(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            Self::OPEN => {
                let last = self.last_failure_ms.load(Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                if now.saturating_sub(last) >= Self::COOLDOWN_SECS * 1000 {
                    self.state.store(Self::HALF_OPEN, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }
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
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
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

        if let Ok(suppressed) = self.suppressed_until.lock()
            && let Some(until) = *suppressed
            && now < until
        {
            return false;
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
    /// Optional OTLP / Datadog export channels. `None` when telemetry is
    /// disabled or no channel is configured.
    channels: Option<TelemetryChannels>,
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
            channels: self.channels.clone(),
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

        // Export channels are configured from the environment and only exist
        // when the user's telemetry opt-in is on. A disabled uploader never
        // constructs them.
        let channels = if config.enabled {
            TelemetryChannels::new(ChannelConfig::from_env())
        } else {
            None
        };

        // The environment variable and the settings file are both triggers;
        // neither can turn the stricter mode off.
        let strict = config.strict || hipaa_enabled();

        Self {
            db_path,
            config,
            client,
            circuit: CircuitBreaker::new(),
            rate: RateLimiter::new(),
            scrubber: Scrubber::new(),
            channels,
            strict,
            verbose: crate::crash_verbose(),
        }
    }

    /// Replace the environment-derived export channel configuration.
    ///
    /// Used by embedders that carry channel settings in their own
    /// configuration and by tests that must not depend on the environment.
    /// Channels remain disabled when the uploader itself is disabled.
    #[must_use]
    pub fn with_channels(mut self, config: ChannelConfig) -> Self {
        self.channels = if self.config.enabled {
            TelemetryChannels::new(config)
        } else {
            None
        };
        self
    }

    /// Whether strict (HIPAA-style) redaction is in force.
    ///
    /// True when the settings file asked for it or `CODEREX_HIPAA` is set.
    #[must_use]
    pub const fn is_strict(&self) -> bool {
        self.strict
    }

    /// Override the resolved modes in tests without touching the environment.
    #[cfg(test)]
    const fn with_mode_for_test(mut self, verbose: bool, strict: bool) -> Self {
        self.verbose = verbose;
        self.strict = strict;
        self
    }

    /// Preview the exact crash payload this uploader would send.
    ///
    /// Uses the uploader's own resolved strict/verbose mode and scrubber, so it
    /// matches the bytes of the next upload rather than re-reading the
    /// environment.
    #[must_use]
    pub fn preview_crash_payload(&self, crash: &PendingCrash) -> serde_json::Value {
        build_crash_payload(&self.scrubber, crash, self.verbose, self.strict)
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
                Ok(()) => self.circuit.on_success(),
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
    ///
    /// When telemetry is disabled this is a no-op: no first-party request and
    /// no export channel request is made, and no row is marked uploaded.
    ///
    /// Crash reports are uploaded even when there are no pending events, so an
    /// eventless or fully sampled-out session does not strand them.
    pub async fn upload_pending(&self) -> Result<(), TelemetryError> {
        if !self.config.enabled {
            return Ok(());
        }

        let store = self.open_store()?;

        let events = store.pending_events(self.config.batch_size)?;
        if events.is_empty() {
            // No events does not mean no telemetry: a sampled-out or eventless
            // session can still have pending crash reports. Upload those before
            // returning so they are not stranded forever.
            return self.upload_pending_crashes().await;
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

        // Fan the same scrubbed events out to the optional export channels.
        // Channel delivery is best-effort: the first-party upload already
        // succeeded and the rows are marked, so a channel outage is logged
        // rather than retried into a duplicate first-party upload.
        if let Some(channels) = &self.channels
            && let Err(e) = channels.send_events(&events).await
        {
            tracing::warn!("telemetry export channel delivery failed: {e}");
        }

        // Also upload crash reports
        self.upload_pending_crashes().await?;

        Ok(())
    }

    async fn upload_pending_crashes(&self) -> Result<(), TelemetryError> {
        // The store owns a rusqlite `Connection`, which is `Send` but not
        // `Sync`; it must be owned here rather than borrowed from the caller,
        // or the future stops being `Send` and cannot be spawned.
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
/// The default path sends the structured panic taxonomy
/// (`panic_template` plus numeric/identifier fields, `panic_message_len`,
/// `panic_message_sha256`) and a symbol-only backtrace (`backtrace_frames`,
/// frame count, symbol length/hash). It never sends `panic_info`, a raw
/// `backtrace`, or any free-form message text. Strict mode
/// (`CODEREX_HIPAA`) additionally drops the symbols.
///
/// Only `CODEREX_CRASH_VERBOSE` (without strict mode) adds `panic_info` — the
/// stored raw text scrubbed again here — and marks the payload
/// `redaction_status: "pattern"`. The raw backtrace is never uploaded; verbose
/// payloads still use symbols only.
///
/// The marker is one of `structured` (metadata + taxonomy), `symbolized`
/// (adds symbol frames) or `pattern` (adds scrubbed free-form text). It is
/// never `clean`, and the keys present always match the marker.
fn build_crash_payload(
    scrubber: &Scrubber,
    crash: &PendingCrash,
    verbose: bool,
    strict: bool,
) -> serde_json::Value {
    let metadata = crate::crash::crash_metadata_from_stored(&crash.panic_info);
    let (classification, symbols) = crate::crash::stored_crash_view(crash);

    let mut payload = serde_json::json!({
        "panic_type": metadata.panic_type,
        "thread_name": metadata.thread_name,
        "panic_location": sanitize_location(&metadata.panic_location, scrubber),
        "os": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": crash.created_at,
        "panic_template": classification.template.as_str(),
        "panic_message_len": classification.message_len,
        "panic_message_sha256": classification.message_sha256,
    });
    for (key, value) in &classification.fields {
        payload[key] = value.clone();
    }

    // Strict mode wins over verbose and over symbol capture.
    let symbolized = if strict {
        None
    } else {
        symbols.filter(|symbols| !symbols.frames.is_empty())
    };
    if let Some(symbols) = &symbolized {
        payload["backtrace_frames"] = serde_json::json!(symbols.frames);
        payload["backtrace_frame_count"] = serde_json::json!(symbols.frame_count);
        payload["backtrace_symbols_len"] = serde_json::json!(symbols.symbols_len);
        payload["backtrace_symbols_sha256"] = serde_json::json!(symbols.symbols_sha256);
    }

    // Free-form text exists only when the row itself holds raw text (verbose
    // ingestion) and verbose is still enabled without strict mode.
    let include_text = !strict
        && verbose
        && !crate::crash::is_structured(&crash.panic_info)
        && !crash.panic_info.is_empty();
    if include_text {
        payload["panic_info"] = serde_json::Value::String(scrubber.scrub(&crash.panic_info));
    }

    let marker = if include_text {
        crate::crash::CRASH_PATTERN_MARKER
    } else if symbolized.is_some() {
        crate::crash::CRASH_SYMBOLIZED_MARKER
    } else {
        crate::crash::CRASH_STRUCTURED_MARKER
    };
    payload["redaction_status"] = serde_json::Value::String(marker.to_string());

    if strict {
        // Every key produced above is on the strict allowlist; this is a
        // defence-in-depth check that should leave the mark intact.
        crate::scrubber::mark_unredacted_if_unallowlisted(&mut payload);
    }
    payload
}

/// Remove absolute-path information from a panic location.
///
/// The location is scrubbed first (home directories and high-entropy path
/// components), then the leading root / Windows drive is removed so no
/// absolute path survives. The file:line:column remainder is still useful for
/// grouping.
fn sanitize_location(location: &str, scrubber: &Scrubber) -> String {
    let scrubbed = scrubber.scrub(location);
    let trimmed = scrubbed.trim();
    let mut relative = trimmed.trim_start_matches('/');
    if relative.len() >= 2 && relative.as_bytes()[1] == b':' {
        relative = &relative[2..];
    }
    let relative = relative.trim_start_matches('\\');
    relative
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(200)
        .collect::<String>()
        .replace(';', ",")
}

/// Preview the exact JSON payload that would be uploaded for `crash`.
///
/// This resolves `CODEREX_CRASH_VERBOSE` and `CODEREX_HIPAA` at call time, so
/// it matches a newly constructed uploader in the same process. Pass the same
/// row the uploader would read (`TelemetryStore::pending_crashes`) and the
/// returned value is the element of the upload's `crashes` array.
///
/// It deliberately returns the *wire* payload, not the local copy: verbose
/// text is pattern-scrubbed exactly as it will be on upload.
#[must_use]
pub fn preview_crash_payload(crash: &PendingCrash) -> serde_json::Value {
    preview_crash_payload_with(crash, crate::crash_verbose(), crate::hipaa_enabled())
}

/// [`preview_crash_payload`] with explicit modes, for callers and tests that
/// must not read the process environment.
#[must_use]
pub fn preview_crash_payload_with(
    crash: &PendingCrash,
    verbose: bool,
    strict: bool,
) -> serde_json::Value {
    build_crash_payload(&Scrubber::new(), crash, verbose, strict)
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
        bytes: encoder.finish().map_err(TelemetryError::Io)?,
        gzipped: true,
    })
}

#[cfg(test)]
mod tests {
    use flate2::read::GzDecoder;
    use std::io::Read;

    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn pending_session_store() -> (tempfile::NamedTempFile, TelemetryStore) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let store = TelemetryStore::open(file.path()).unwrap();
        store
            .insert(&crate::TelemetryEvent::SessionStart {
                session_id: "channel-test".into(),
                timestamp: chrono::Utc::now(),
            })
            .unwrap();
        (file, store)
    }

    /// Settings are a second trigger for strict redaction, alongside
    /// `CODEREX_HIPAA`. Either one turns it on; neither can turn it off.
    #[test]
    fn a_strict_config_turns_on_strict_redaction() {
        let dir = tempfile::tempdir().unwrap();
        let uploader = TelemetryUploader::new(
            dir.path().join("strict.db"),
            TelemetryConfig {
                strict: true,
                ..Default::default()
            },
        );
        assert!(uploader.is_strict());
    }

    #[tokio::test]
    async fn disabled_uploader_sends_nothing_and_keeps_rows_pending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (file, store) = pending_session_store();
        drop(store);

        let uploader = TelemetryUploader::new(
            file.path().to_path_buf(),
            TelemetryConfig {
                endpoint: format!("{}/events", server.uri()),
                enabled: false,
                ..Default::default()
            },
        )
        .with_channels(ChannelConfig {
            otlp_endpoint: Some(server.uri()),
            datadog_api_key: Some("dd-key".into()),
            datadog_endpoint: Some(format!("{}/api/v2/logs", server.uri())),
            ..Default::default()
        });

        uploader.upload_pending().await.unwrap();
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a disabled uploader must not contact the first-party endpoint or any channel"
        );

        let store = TelemetryStore::open(file.path()).unwrap();
        assert_eq!(
            store.pending_count().unwrap(),
            1,
            "a disabled uploader must not mark the row uploaded"
        );
    }

    /// A session that recorded no events (for example every event was sampled
    /// out) must still upload its pending crash reports. The empty-event early
    /// return used to skip them forever.
    #[tokio::test]
    async fn pending_crash_is_uploaded_when_there_are_no_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/crashes"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let store = TelemetryStore::open(file.path()).unwrap();
            store
                .insert_crash("panicked at src/main.rs:1:1:\nboom", None)
                .unwrap();
            assert_eq!(store.event_count().unwrap(), 0);
        }

        let uploader = TelemetryUploader::new(
            file.path().to_path_buf(),
            TelemetryConfig {
                endpoint: format!("{}/events", server.uri()),
                crash_endpoint: Some(format!("{}/crashes", server.uri())),
                enabled: true,
                ..Default::default()
            },
        );

        uploader.upload_pending().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "the crash request must be attempted");
        assert_eq!(requests[0].url.path(), "/crashes");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["crashes"].as_array().unwrap().len(), 1);

        let store = TelemetryStore::open(file.path()).unwrap();
        assert_eq!(store.pending_crashes(10).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn enabled_uploader_fans_events_out_to_channels() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/events"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/logs"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (file, store) = pending_session_store();
        drop(store);

        let uploader = TelemetryUploader::new(
            file.path().to_path_buf(),
            TelemetryConfig {
                endpoint: format!("{}/events", server.uri()),
                enabled: true,
                ..Default::default()
            },
        )
        .with_channels(ChannelConfig {
            otlp_endpoint: Some(server.uri()),
            ..Default::default()
        });

        uploader.upload_pending().await.unwrap();

        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| request.url.path().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "/events"), "{paths:?}");
        assert!(paths.iter().any(|p| p == "/v1/logs"), "{paths:?}");

        let store = TelemetryStore::open(file.path()).unwrap();
        assert_eq!(store.pending_count().unwrap(), 0);
    }

    #[test]
    fn telemetry_config_defaults() {
        let config = TelemetryConfig::default();
        assert!(!config.enabled, "telemetry must be opted into");
        assert!(
            config.endpoint.is_empty(),
            "there is no default destination"
        );
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
        let config = TelemetryConfig {
            endpoint: "https://telemetry.example.com/v1/events".into(),
            ..TelemetryConfig::default()
        };
        assert_eq!(
            config.resolve_crash_endpoint(),
            "https://telemetry.example.com/v1/crashes"
        );
        // With no destination configured nothing is derived.
        assert!(
            TelemetryConfig::default()
                .resolve_crash_endpoint()
                .is_empty()
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
            panic_template: None,
            panic_fields: None,
            panic_message_len: None,
            panic_message_sha256: None,
            backtrace_symbols: None,
            backtrace_frame_count: None,
            backtrace_symbols_len: None,
            backtrace_symbols_sha256: None,
        }
    }

    fn pending_classified_crash(
        panic_info: &str,
        template: &str,
        fields: &str,
        symbols: Option<&str>,
    ) -> PendingCrash {
        let frames = symbols.map(crate::symbolize_backtrace);
        PendingCrash {
            id: 1,
            panic_info: panic_info.into(),
            backtrace: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            panic_template: Some(template.into()),
            panic_fields: Some(fields.into()),
            panic_message_len: Some(42),
            panic_message_sha256: Some("a".repeat(64)),
            backtrace_symbols: frames
                .as_ref()
                .and_then(|symbols| serde_json::to_string(&symbols.frames).ok()),
            backtrace_frame_count: frames
                .as_ref()
                .map(|symbols| i64::try_from(symbols.frame_count).unwrap_or(i64::MAX)),
            backtrace_symbols_len: frames
                .as_ref()
                .map(|symbols| i64::try_from(symbols.symbols_len).unwrap_or(i64::MAX)),
            backtrace_symbols_sha256: frames.map(|symbols| symbols.symbols_sha256),
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
        assert!(payload.get("message").is_none(), "{payload}");
        assert_eq!(payload["panic_template"], serde_json::json!("unknown"));
        assert!(
            payload["panic_message_len"].is_number(),
            "the length must be sent even for unknown messages: {payload}"
        );
        assert!(
            payload["panic_message_sha256"].is_string(),
            "the hash must be sent even for unknown messages: {payload}"
        );
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
    /// scrubbed first and marked `pattern` — never claimed clean. The raw
    /// backtrace is still not sent; only symbol frames are.
    #[test]
    fn verbose_crash_payload_is_pattern_scrubbed() {
        let scrubber = Scrubber::new();
        let crash = pending_crash_with(
            "panicked at src/main.rs:10:5:\nfailed for bob@example.com carrying A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            Some(
                "   0: coderex_core::engine::run\n             at /home/bob/project/crates/core/src/engine.rs:42:9\n   1: at file.rs:12:9",
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
        assert!(
            payload.get("backtrace").is_none(),
            "the raw backtrace must never be uploaded: {payload}"
        );
        assert_eq!(
            payload["backtrace_frames"],
            serde_json::json!(["coderex_core::engine::run"])
        );

        let rendered = payload.to_string();
        for forbidden in [
            "bob@example.com",
            "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            "/home/bob",
            "engine.rs:42:9",
        ] {
            assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
        }
        for expected in [
            "failed for",
            "coderex_core::engine::run",
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

    /// A classified row with symbols is marked `symbolized`: the payload keys
    /// match the marker (no free-form key anywhere).
    #[test]
    fn symbolized_crash_payload_has_symbols_and_no_paths() {
        let backtrace = "   0: std::panicking::begin_panic_handler\n             at /rustc/abcdef0123456789abcdef0123456789/library/std/src/panicking.rs:123:5\n   1: coderex_core::engine::QueryEngine::submit\n             at /home/somebody/project/crates/core/src/engine.rs:42:9\n";
        let crash = pending_classified_crash(
            "[strict-crash-summary] type=&str; thread=main; location=src/main.rs:10:5; template=index_out_of_bounds; message_len=42; message_sha256=aaa; message=omitted; backtrace=omitted",
            "index_out_of_bounds",
            r#"{"panic_container_len":3,"panic_index":7}"#,
            Some(backtrace),
        );
        let payload = build_crash_payload(&Scrubber::new(), &crash, false, false);

        assert_eq!(payload["redaction_status"], serde_json::json!("symbolized"));
        assert_eq!(
            payload["panic_template"],
            serde_json::json!("index_out_of_bounds")
        );
        assert_eq!(payload["panic_container_len"], serde_json::json!(3));
        assert_eq!(payload["panic_index"], serde_json::json!(7));
        assert_eq!(payload["backtrace_frame_count"], serde_json::json!(2));
        assert!(payload.get("panic_info").is_none(), "{payload}");
        assert!(payload.get("backtrace").is_none(), "{payload}");
        assert!(payload.get("message").is_none(), "{payload}");

        let rendered = payload.to_string();
        for forbidden in ["/rustc", "/home", "somebody", "panicking.rs", "engine.rs"] {
            assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
        }
        assert!(rendered.contains("QueryEngine::submit"), "{rendered}");
    }

    /// An unknown message contributes only its length and SHA-256; no text and
    /// no guessed fields reach the wire.
    #[test]
    fn unknown_message_sends_only_length_and_hash() {
        let message = "user said something private that is not a known panic shape";
        let classification = crate::classify_panic_message(message);
        let crash = pending_crash_with(&format!("panicked at src/a.rs:1:1:\n{message}"), None);
        let payload = build_crash_payload(&Scrubber::new(), &crash, false, false);

        assert_eq!(payload["panic_template"], serde_json::json!("unknown"));
        assert_eq!(
            payload["panic_message_len"],
            serde_json::json!(classification.message_len)
        );
        assert_eq!(
            payload["panic_message_sha256"],
            serde_json::json!(classification.message_sha256)
        );
        let rendered = payload.to_string();
        assert!(!rendered.contains("user said"), "{rendered}");
        assert!(!rendered.contains("private"), "{rendered}");
        for key in ["panic_index", "panic_container_len", "panic_error_type"] {
            assert!(payload.get(key).is_none(), "{key}: {payload}");
        }
    }

    /// A pending file or database row is local input, not a trusted source. A
    /// tampered classification blob must not smuggle prose, paths, or a fake
    /// fingerprint onto the wire; only the fixed key set and value shapes the
    /// classifier itself produces survive.
    #[test]
    fn tampered_pending_row_cannot_smuggle_free_form_text() {
        let mut crash = pending_classified_crash(
            "[strict-crash-summary] type=&str; thread=main; location=src/main.rs:1:1; message=omitted; backtrace=omitted",
            "index_out_of_bounds",
            r#"{"panic_index":7,"panic_container_len":"/home/bob/secret","panic_error_type":"USER SAID SECRET","free_form":"hello bob@example.com","panic_operation":["add"]}"#,
            None,
        );
        crash.backtrace_symbols = Some(
            serde_json::to_string(&vec![
                "coderex_core::engine::run".to_string(),
                "/home/bob/private.rs:1:1".to_string(),
                "std::thread::spawn::<user@example.com>".to_string(),
            ])
            .unwrap(),
        );
        crash.backtrace_frame_count = Some(9);
        crash.panic_message_sha256 = Some("not a hash: private text".into());

        let payload = build_crash_payload(&Scrubber::new(), &crash, false, false);

        let rendered = payload.to_string();
        for forbidden in [
            "/home/bob",
            "private.rs",
            "USER SAID",
            "bob@example.com",
            "not a hash",
            "hello",
        ] {
            assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
        }
        assert_eq!(payload["panic_index"], serde_json::json!(7));
        assert!(payload.get("panic_container_len").is_none(), "{payload}");
        assert!(payload.get("panic_error_type").is_none(), "{payload}");
        assert!(payload.get("free_form").is_none(), "{payload}");
        assert!(payload.get("panic_operation").is_none(), "{payload}");
        assert_eq!(payload["panic_message_sha256"], serde_json::json!(""));
        assert_eq!(
            payload["backtrace_frames"],
            serde_json::json!(["coderex_core::engine::run"])
        );
        assert_eq!(payload["backtrace_frame_count"], serde_json::json!(9));
        assert_eq!(payload["redaction_status"], serde_json::json!("symbolized"));
    }

    /// The preview API returns exactly what the upload path builds — including
    /// the verbose scrub — so a user can see the wire form before enabling it.
    #[test]
    fn preview_shows_the_exact_scrubbed_wire_payload() {
        let crash = pending_crash_with(
            "panicked at src/main.rs:10:5:\nfailed for bob@example.com carrying A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            Some("   0: coderex_core::engine::run\n             at /home/bob/x.rs:1:1\n"),
        );

        let preview = preview_crash_payload_with(&crash, true, false);
        let uploaded = build_crash_payload(&Scrubber::new(), &crash, true, false);
        assert_eq!(preview, uploaded, "preview must match the wire payload");
        assert_eq!(preview["redaction_status"], serde_json::json!("pattern"));
        let rendered = preview.to_string();
        assert!(!rendered.contains("bob@example.com"), "{rendered}");
        assert!(!rendered.contains("A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6"));
        assert!(!rendered.contains("/home/bob"), "{rendered}");
        assert!(rendered.contains("<EMAIL>"), "{rendered}");
        assert!(rendered.contains("<SECRET>"), "{rendered}");
    }

    /// HIPAA wins over verbose in the preview: structured-only, no text, no
    /// symbols.
    #[test]
    fn preview_under_hipaa_is_structured_even_when_verbose() {
        let crash = pending_classified_crash(
            "panicked at src/main.rs:10:5:\nuser said something private",
            "index_out_of_bounds",
            r#"{"panic_container_len":3,"panic_index":7}"#,
            Some("   0: coderex_core::engine::run\n"),
        );
        let preview = preview_crash_payload_with(&crash, true, true);

        assert_eq!(preview["redaction_status"], serde_json::json!("structured"));
        assert!(preview.get("panic_info").is_none(), "{preview}");
        assert!(preview.get("backtrace_frames").is_none(), "{preview}");
        assert!(preview.get("backtrace").is_none(), "{preview}");
        let rendered = preview.to_string();
        assert!(!rendered.contains("user said"), "{rendered}");
        assert!(
            !rendered.contains("coderex_core::engine::run"),
            "{rendered}"
        );
        // The strict allowlist must accept every key we produce.
        assert_ne!(
            preview["redaction_status"],
            serde_json::json!("unredacted"),
            "{preview}"
        );
    }

    /// The uploader-bound preview uses the uploader's resolved modes, so it is
    /// byte-identical to what that uploader sends.
    #[test]
    fn uploader_preview_uses_its_resolved_mode() {
        let crash = pending_crash_with(
            "panicked at src/main.rs:10:5:\nfailed for bob@example.com",
            None,
        );
        let uploader = TelemetryUploader::new(
            std::path::PathBuf::from("/tmp/coderex-preview.db"),
            TelemetryConfig::default(),
        )
        .with_mode_for_test(false, true);
        let preview = uploader.preview_crash_payload(&crash);
        assert_eq!(preview["redaction_status"], serde_json::json!("structured"));
        assert!(preview.get("panic_info").is_none(), "{preview}");
    }

    /// A legacy raw row never uploads its text by default; only the taxonomy
    /// fingerprint is sent.
    #[test]
    fn legacy_row_default_never_uploads_free_form() {
        let crash = pending_crash_with(
            "panicked at src/old.rs:3:1:\nuser said something private",
            Some("   0: coderex_core::engine::run\n             at /home/bob/secret.rs:3:1\n"),
        );
        let payload = build_crash_payload(&Scrubber::new(), &crash, false, false);
        let rendered = payload.to_string();
        assert!(!rendered.contains("user said"), "{rendered}");
        assert!(!rendered.contains("/home/bob"), "{rendered}");
        assert!(!rendered.contains("secret.rs"), "{rendered}");
        assert_eq!(payload["redaction_status"], serde_json::json!("symbolized"));
    }

    /// A panic location with an absolute path is made relative and the home
    /// user is redacted.
    #[test]
    fn payload_has_no_absolute_location_path() {
        let crash = pending_crash_with(
            "[strict-crash-summary] type=&str; thread=main; location=/Users/somebody/project/src/main.rs:10:5; message=omitted; backtrace=omitted",
            None,
        );
        let payload = build_crash_payload(&Scrubber::new(), &crash, false, false);
        let location = payload["panic_location"].as_str().unwrap();
        assert!(!location.starts_with('/'), "{location}");
        assert!(!location.contains("somebody"), "{location}");
        assert!(location.ends_with("src/main.rs:10:5"), "{location}");
    }
}
