//! Crash reporting — panic hook integration.
//!
//! Crash reports are written to a pending directory during a panic (since
//! `rusqlite::Connection` is not `Sync` and cannot be used inside a panic
//! hook). On the next startup the telemetry system scans the pending
//! directory and ingests the files into the SQLite store.
//!
//! # What leaves the machine
//!
//! The free-form panic payload is conversation-adjacent text, so it is not
//! uploaded as text unless the user opts in with `CODEREX_CRASH_VERBOSE`. Even
//! then the raw text is only ever a *local* copy (`0600`, pruned by age); the
//! upload re-scrubs it and marks it `redaction_status: "pattern"`.
//!
//! The default path uploads a *structured* representation instead:
//!
//! - the panic message is classified into a fixed [`PanicTemplate`] plus a few
//!   safe parsed fields (an index, a slice bound, an operation name, an error
//!   type identifier) — never the message itself; and
//! - the backtrace is reduced to symbol-only frame names (up to
//!   [`MAX_BACKTRACE_FRAMES`]), with `at <path>` lines, usernames and source
//!   snippets removed.
//!
//! An unrecognised message contributes only its byte length and SHA-256. The
//! payload's `redaction_status` is always one of `structured`, `symbolized` or
//! `pattern` — never `clean`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::store::CrashRecord;

/// Prefix that identifies a structured crash summary.
///
/// A summary carrying this prefix contains only panic metadata (type, thread,
/// location), the panic template and message length/hash — never the free-form
/// panic payload or backtrace.
pub const CRASH_SUMMARY_PREFIX: &str = "[strict-crash-summary]";

/// Payload value written under `redaction_status` when only structured panic
/// metadata is sent.
pub const CRASH_STRUCTURED_MARKER: &str = "structured";

/// Payload value written under `redaction_status` when the payload carries a
/// symbol-only backtrace (`backtrace_frames`) and no free-form text.
pub const CRASH_SYMBOLIZED_MARKER: &str = "symbolized";

/// Payload value written under `redaction_status` when free-form panic text is
/// sent after pattern scrubbing.
///
/// This deliberately does *not* claim the payload is clean: the scrubber
/// removes shapes, not meaning.
pub const CRASH_PATTERN_MARKER: &str = "pattern";

/// Environment variable that opts a crash report into free-form diagnostics.
///
/// Present with any value other than `0`/`false`/`no`/empty includes the panic
/// message (scrubbed at upload). The backtrace is still uploaded as symbols
/// only; the raw backtrace remains a local copy. [`crate::hipaa_enabled`]
/// (`CODEREX_HIPAA`) always wins: even when this is set, strict mode sends
/// structured metadata only.
pub const CRASH_VERBOSE_ENV: &str = "CODEREX_CRASH_VERBOSE";

/// Maximum number of backtrace frames included in an upload.
///
/// Backtraces are diagnostics, not a complete stack dump; the top frames carry
/// the actionable information and the cap keeps a pathological stack from
/// producing an unbounded payload. The full symbol list is never stored.
pub const MAX_BACKTRACE_FRAMES: usize = 24;

/// Maximum characters kept from one backtrace symbol.
const MAX_SYMBOL_CHARS: usize = 240;

/// Pending crash files older than this are deleted without ingestion.
///
/// The pending directory can contain raw verbose text; a file that was never
/// ingested because the process did not start telemetry again must not live
/// forever. Seven days mirrors a typical "user restarts within a week" window.
pub const DEFAULT_PENDING_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// Locally stored crash records older than this are pruned on ingestion.
///
/// This bounds the user-only raw copy kept for verbose diagnostics (30 days).
pub const DEFAULT_CRASH_RETENTION_DAYS: i64 = 30;

/// Process-global switch for panic-time crash capture.
///
/// Defaults to `false`: installing the hook is not enough to make an opted-out
/// process write crash files. The host turns this on after settings resolve
/// (see [`set_capture_enabled`]).
static CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable panic-time crash capture for this process.
///
/// When disabled (the default), an installed panic hook calls the previous hook
/// and writes nothing to the pending directory. Call this with `true` after the
/// telemetry settings resolve to opt the session in; callers that run with
/// telemetry disabled leave it off and can still prune old files with
/// [`prune_pending`].
pub fn set_capture_enabled(enabled: bool) {
    CAPTURE_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether panic-time crash capture is currently enabled.
#[must_use]
pub fn capture_enabled() -> bool {
    CAPTURE_ENABLED.load(Ordering::Relaxed)
}

/// Whether free-form crash diagnostics are requested for this process.
#[must_use]
pub fn crash_verbose() -> bool {
    crash_verbose_with(std::env::var(CRASH_VERBOSE_ENV).ok().as_deref())
}

/// Pure helper behind [`crash_verbose`]; testable without touching the process
/// environment.
#[must_use]
pub fn crash_verbose_with(value: Option<&str>) -> bool {
    value.is_some_and(|raw| {
        let trimmed = raw.trim();
        !(trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("0")
            || trimmed.eq_ignore_ascii_case("false")
            || trimmed.eq_ignore_ascii_case("no"))
    })
}

// ─── Panic taxonomy ─────────────────────────────────────────────────────────

/// A known panic shape.
///
/// Classification is text-shape based: it recognises the messages the standard
/// library and common lock APIs emit. It is deliberately not an attempt to
/// understand arbitrary prose — an unmatched message is
/// [`PanicTemplate::Unknown`] and contributes only a length and hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanicTemplate {
    /// `Result::unwrap()` on an `Err`.
    UnwrapErr,
    /// `Option::unwrap()` on a `None`.
    UnwrapNone,
    /// `Result::expect()` on an `Err` (non-std marker; std emits a custom
    /// prefix and lands in `unknown`).
    ExpectErr,
    /// `Option::expect()` on a `None` (non-std marker; see `ExpectErr`).
    ExpectNone,
    /// Slice/vector index out of bounds.
    IndexOutOfBounds,
    /// Slice range out of bounds.
    SliceRange,
    /// Allocation capacity overflow.
    CapacityOverflow,
    /// Arithmetic overflow or division by zero.
    ArithmeticOverflow,
    /// `assert!` / `assert_eq!` / `assert_ne!` failure.
    AssertionFailed,
    /// The bare `panic!()` message (`"explicit panic"`).
    ExplicitPanic,
    /// A poisoned mutex/rwlock.
    PoisonedLock,
    /// Anything else. Only its length and SHA-256 are sent.
    Unknown,
}

impl PanicTemplate {
    /// Stable wire/database identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnwrapErr => "unwrap_err",
            Self::UnwrapNone => "unwrap_none",
            Self::ExpectErr => "expect_err",
            Self::ExpectNone => "expect_none",
            Self::IndexOutOfBounds => "index_out_of_bounds",
            Self::SliceRange => "slice_range",
            Self::CapacityOverflow => "capacity_overflow",
            Self::ArithmeticOverflow => "arithmetic_overflow",
            Self::AssertionFailed => "assertion_failed",
            Self::ExplicitPanic => "explicit_panic",
            Self::PoisonedLock => "poisoned_lock",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stable identifier produced by [`Self::as_str`].
    ///
    /// Unrecognised values (including database rows written by a future
    /// version) fall back to [`PanicTemplate::Unknown`].
    #[must_use]
    pub fn from_identifier(value: &str) -> Self {
        match value {
            "unwrap_err" => Self::UnwrapErr,
            "unwrap_none" => Self::UnwrapNone,
            "expect_err" => Self::ExpectErr,
            "expect_none" => Self::ExpectNone,
            "index_out_of_bounds" => Self::IndexOutOfBounds,
            "slice_range" => Self::SliceRange,
            "capacity_overflow" => Self::CapacityOverflow,
            "arithmetic_overflow" => Self::ArithmeticOverflow,
            "assertion_failed" => Self::AssertionFailed,
            "explicit_panic" => Self::ExplicitPanic,
            "poisoned_lock" => Self::PoisonedLock,
            _ => Self::Unknown,
        }
    }

    /// Whether the template carries safe parsed fields.
    #[must_use]
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// Structured classification of a panic message.
///
/// `fields` only ever contains values parsed from fixed numeric/identifier
/// shapes (`panic_index`, `panic_container_len`, `panic_range_start`,
/// `panic_range_end`, `panic_slice_len`, `panic_operation`,
/// `panic_error_type`). No free-form text is stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PanicClassification {
    /// The recognised template, or [`PanicTemplate::Unknown`].
    pub template: PanicTemplate,
    /// Safe parsed fields (payload key → value).
    #[serde(default)]
    pub fields: BTreeMap<String, serde_json::Value>,
    /// Length of the raw panic message in bytes.
    pub message_len: usize,
    /// SHA-256 of the raw panic message, lowercase hex.
    pub message_sha256: String,
}

impl PanicClassification {
    /// A classification for a report that has no recoverable message (for
    /// example an old structured summary whose payload was omitted).
    ///
    /// The length is zero and the hash is empty: no message existed, so there
    /// is nothing to fingerprint.
    #[must_use]
    pub(crate) const fn empty() -> Self {
        Self {
            template: PanicTemplate::Unknown,
            fields: BTreeMap::new(),
            message_len: 0,
            message_sha256: String::new(),
        }
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn index_oob_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^index out of bounds: the len is (\d+) but the index is (\d+)")
            .expect("index-out-of-bounds regex is valid")
    })
}

fn slice_range_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^range (start|end) index (\d+) out of range for slice of length (\d+)")
            .expect("slice-range regex is valid")
    })
}

fn slice_start_end_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^slice index starts at (\d+) but ends at (\d+)")
            .expect("slice-start-end regex is valid")
    })
}

/// The arithmetic operation named by a standard overflow panic, if any.
fn arithmetic_operation(message: &str) -> Option<&'static str> {
    const MESSAGES: [(&str, &str); 10] = [
        ("attempt to add with overflow", "add"),
        ("attempt to subtract with overflow", "sub"),
        ("attempt to multiply with overflow", "mul"),
        ("attempt to divide with overflow", "div_overflow"),
        ("attempt to divide by zero", "div_zero"),
        (
            "attempt to calculate the remainder with overflow",
            "rem_overflow",
        ),
        (
            "attempt to calculate the remainder with a divisor of zero",
            "rem_zero",
        ),
        ("attempt to negate with overflow", "neg"),
        ("attempt to shift left with overflow", "shift_left"),
        ("attempt to shift right with overflow", "shift_right"),
    ];
    MESSAGES
        .iter()
        .find(|(prefix, _)| message.starts_with(prefix))
        .map(|(_, operation)| *operation)
}

/// Extract an error *type name* from a `Result::unwrap`/`expect` `Err` Debug
/// string.
///
/// Only an identifier-like type name is kept. A tuple/struct Debug such as
/// `MyError("user said …")` yields `MyError`; a string Debug (`"…"`) or an
/// arbitrary prose word does not qualify. The character immediately after the
/// name must be a struct/tuple delimiter (so `USER SAID …` is rejected).
fn extract_error_type(message: &str) -> Option<String> {
    const PREFIXES: [&str; 2] = [
        "called `Result::unwrap()` on an `Err` value: ",
        "called `Result::expect()` on an `Err` value: ",
    ];
    let rest = PREFIXES
        .iter()
        .find_map(|prefix| message.strip_prefix(*prefix))?;

    let mut end = 0;
    for (index, ch) in rest.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 || end > 100 {
        return None;
    }
    let name = &rest[..end];
    if !name.starts_with(|ch: char| ch.is_ascii_uppercase()) {
        return None;
    }
    let tail = rest[end..].trim_start();
    if !(tail.is_empty() || tail.starts_with('{') || tail.starts_with('(') || tail.starts_with('['))
    {
        return None;
    }
    Some(name.to_string())
}

/// Classify a panic message into a known template and safe parsed fields.
///
/// The raw message is *not* retained. The returned classification is all that
/// the rest of the crash pipeline needs.
#[must_use]
pub fn classify_panic_message(message: &str) -> PanicClassification {
    let trimmed = message.trim();
    let template = detect_template(trimmed);
    let mut fields = BTreeMap::new();

    match template {
        PanicTemplate::IndexOutOfBounds => {
            if let Some(caps) = index_oob_regex().captures(trimmed) {
                if let Ok(len) = caps[1].parse::<u64>() {
                    fields.insert("panic_container_len".into(), serde_json::json!(len));
                }
                if let Ok(index) = caps[2].parse::<u64>() {
                    fields.insert("panic_index".into(), serde_json::json!(index));
                }
            }
        }
        PanicTemplate::SliceRange => {
            if let Some(caps) = slice_range_regex().captures(trimmed) {
                let key = if &caps[1] == "start" {
                    "panic_range_start"
                } else {
                    "panic_range_end"
                };
                if let Ok(bound) = caps[2].parse::<u64>() {
                    fields.insert(key.into(), serde_json::json!(bound));
                }
                if let Ok(len) = caps[3].parse::<u64>() {
                    fields.insert("panic_slice_len".into(), serde_json::json!(len));
                }
            } else if let Some(caps) = slice_start_end_regex().captures(trimmed) {
                if let Ok(start) = caps[1].parse::<u64>() {
                    fields.insert("panic_range_start".into(), serde_json::json!(start));
                }
                if let Ok(end) = caps[2].parse::<u64>() {
                    fields.insert("panic_range_end".into(), serde_json::json!(end));
                }
            }
        }
        PanicTemplate::ArithmeticOverflow => {
            if let Some(operation) = arithmetic_operation(trimmed) {
                fields.insert("panic_operation".into(), serde_json::json!(operation));
            }
        }
        PanicTemplate::UnwrapErr | PanicTemplate::ExpectErr => {
            if let Some(error_type) = extract_error_type(trimmed) {
                fields.insert("panic_error_type".into(), serde_json::json!(error_type));
            }
        }
        _ => {}
    }

    PanicClassification {
        template,
        fields,
        message_len: message.len(),
        message_sha256: sha256_hex(message.as_bytes()),
    }
}

fn detect_template(message: &str) -> PanicTemplate {
    // Poison detection runs first: `Mutex::lock().unwrap()` on a poisoned lock
    // produces the ordinary `Result::unwrap` prefix with `PoisonError` Debug.
    if message.contains("PoisonError") || message.starts_with("poisoned lock") {
        return PanicTemplate::PoisonedLock;
    }
    if message.starts_with("called `Result::unwrap()` on an `Err` value") {
        return PanicTemplate::UnwrapErr;
    }
    if message.starts_with("called `Result::expect()` on an `Err` value") {
        return PanicTemplate::ExpectErr;
    }
    if message.starts_with("called `Option::unwrap()` on a `None` value") {
        return PanicTemplate::UnwrapNone;
    }
    if message.starts_with("called `Option::expect()` on a `None` value") {
        return PanicTemplate::ExpectNone;
    }
    if index_oob_regex().is_match(message) {
        return PanicTemplate::IndexOutOfBounds;
    }
    if slice_range_regex().is_match(message) || slice_start_end_regex().is_match(message) {
        return PanicTemplate::SliceRange;
    }
    if message.starts_with("capacity overflow") {
        return PanicTemplate::CapacityOverflow;
    }
    if arithmetic_operation(message).is_some() {
        return PanicTemplate::ArithmeticOverflow;
    }
    if message.starts_with("assertion failed")
        || (message.starts_with("assertion `") && message.contains(" failed"))
    {
        return PanicTemplate::AssertionFailed;
    }
    if message == "explicit panic" {
        return PanicTemplate::ExplicitPanic;
    }
    PanicTemplate::Unknown
}

/// Extract the message portion of a standard `panicked at <location>:` display
/// string, falling back to the whole input.
fn panic_message_text(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("panicked at ")
        && let Some((_, message)) = rest.split_once(":\n")
    {
        return message;
    }
    text
}

// ─── Symbol-only backtraces ─────────────────────────────────────────────────

/// Symbol-only representation of a backtrace.
///
/// `frames` holds at most [`MAX_BACKTRACE_FRAMES`] sanitized symbols. `len` and
/// `sha256` describe exactly `frames.join("\n")`, so a receiver can fingerprint
/// what it was sent without seeing paths or source text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct BacktraceSymbols {
    /// Sanitized symbols, top frames first (at most [`MAX_BACKTRACE_FRAMES`]).
    pub frames: Vec<String>,
    /// Total number of symbol-bearing frames parsed before truncation.
    pub frame_count: usize,
    /// Byte length of `frames.join("\n")`.
    pub symbols_len: usize,
    /// SHA-256 (lowercase hex) of `frames.join("\n")`.
    pub symbols_sha256: String,
}

/// Reduce a `std::backtrace::Backtrace` debug string to symbols.
///
/// Frame lines (`   0: symbol`) are kept; `at <path>:line` continuation lines,
/// `note:` lines and anything else are dropped. A symbol that contains path
/// punctuation (`/`, `\`, `@`, `~`) is dropped rather than truncated, so an
/// absolute path can never appear in the result. `<unknown>` frames carry no
/// symbol and are skipped.
#[must_use]
pub fn symbolize_backtrace(raw: &str) -> BacktraceSymbols {
    symbolize_backtrace_limited(raw, MAX_BACKTRACE_FRAMES)
}

/// [`symbolize_backtrace`] with an explicit frame cap (used by tests).
#[must_use]
pub fn symbolize_backtrace_limited(raw: &str, max_frames: usize) -> BacktraceSymbols {
    let mut frames = Vec::new();
    let mut frame_count = 0;
    for line in raw.lines() {
        if let Some(symbol) = frame_symbol(line) {
            frame_count += 1;
            if frames.len() < max_frames {
                frames.push(symbol);
            }
        }
    }
    let joined = frames.join("\n");
    BacktraceSymbols {
        symbols_len: joined.len(),
        symbols_sha256: sha256_hex(joined.as_bytes()),
        frames,
        frame_count,
    }
}

/// Extract the sanitized symbol from one backtrace line, if it is a frame.
fn frame_symbol(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    // Continuation lines point at source files; drop them entirely.
    if trimmed.starts_with("at ") || trimmed.starts_with("note:") {
        return None;
    }
    let (index, rest) = trimmed.split_once(':')?;
    if index.is_empty() || !index.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let rest = rest.trim();
    // Some renderers repeat the continuation marker on a numbered line.
    if rest.starts_with("at ") || rest.starts_with("note:") {
        return None;
    }
    // Older formats: `0x123456 - module::symbol`.
    let symbol = rest
        .split_once(" - ")
        .map_or(rest, |(_, symbol)| symbol.trim());
    sanitize_symbol(symbol)
}

fn sanitize_symbol(symbol: &str) -> Option<String> {
    let symbol = symbol.trim();
    if symbol.is_empty() || symbol == "<unknown>" {
        return None;
    }
    // Path punctuation means this is not a symbol (or is a symbol the runtime
    // failed to demangle); dropping it is safer than sending a path.
    if symbol
        .chars()
        .any(|ch| matches!(ch, '/' | '\\' | '@' | '~'))
    {
        return None;
    }
    let cleaned: String = symbol
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(collapsed.chars().take(MAX_SYMBOL_CHARS).collect())
}

// ─── Crash report capture ───────────────────────────────────────────────────

/// A captured crash report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrashReport {
    /// Panic info message.
    ///
    /// In verbose mode this is the free-form `format!("{info}")` text, which may
    /// contain whatever the panicking code carried — a *local* copy only. In
    /// every other mode it is a [`CRASH_SUMMARY_PREFIX`] summary.
    pub panic_info: String,
    /// Raw backtrace at time of panic.
    ///
    /// Only present in verbose mode (a local copy). It is never uploaded; the
    /// upload uses [`Self::backtrace_symbols`] instead.
    pub backtrace: Option<String>,
    /// When the crash occurred.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Panic source location (`file:line:column`), when the runtime knows it.
    ///
    /// Defaulted on deserialization so pending files written by older builds
    /// still load.
    #[serde(default)]
    pub panic_location: Option<String>,
    /// Name of the panicking thread, when it has one.
    #[serde(default)]
    pub thread_name: Option<String>,
    /// Panic payload type (`&str`, `String`, or `unknown`).
    #[serde(default)]
    pub panic_type: Option<String>,
    /// Wire identifier of the classified panic template.
    #[serde(default)]
    pub panic_template: Option<String>,
    /// Safe parsed fields from the classification.
    #[serde(default)]
    pub panic_fields: Option<serde_json::Map<String, serde_json::Value>>,
    /// Byte length of the raw panic message.
    #[serde(default)]
    pub panic_message_len: Option<usize>,
    /// SHA-256 (lowercase hex) of the raw panic message.
    #[serde(default)]
    pub panic_message_sha256: Option<String>,
    /// Sanitized backtrace frame symbols (top [`MAX_BACKTRACE_FRAMES`]).
    #[serde(default)]
    pub backtrace_symbols: Option<Vec<String>>,
    /// Total number of symbol-bearing frames parsed.
    #[serde(default)]
    pub backtrace_frame_count: Option<usize>,
    /// Byte length of the joined symbol representation.
    #[serde(default)]
    pub backtrace_symbols_len: Option<usize>,
    /// SHA-256 of the joined symbol representation.
    #[serde(default)]
    pub backtrace_symbols_sha256: Option<String>,
}

/// Fold a value to one line and cap its length.
fn one_line(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect()
}

/// Fold a metadata value to a single line and remove the `;` separator used by
/// the summary format, so a hostile thread name cannot forge a field.
fn structured_field(value: &str, max_chars: usize) -> String {
    one_line(value, max_chars).replace(';', ",")
}

/// Extract `file:line:column` from the standard panic display prefix
/// (`panicked at <location>:\n<message>`), without keeping the message.
fn panic_location_from_text(text: &str) -> Option<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^panicked at ([^\n]+?:\d+:\d+):")
            .expect("panic location regex is valid")
    });
    re.captures(text)
        .and_then(|caps| caps.get(1))
        .map(|location| one_line(location.as_str(), 200))
}

/// Build the strict-mode structured summary for a captured report.
///
/// Only panic type, thread name, location, template and message fingerprint
/// are retained. The panic payload (which may contain conversation text) and
/// the backtrace (which may contain paths and source lines) are replaced by the
/// literal `omitted`.
pub fn structured_summary(report: &CrashReport) -> String {
    let location = report
        .panic_location
        .as_deref()
        .map(|value| structured_field(value, 200))
        .or_else(|| {
            panic_location_from_text(&report.panic_info)
                .map(|location| structured_field(&location, 200))
        })
        .unwrap_or_else(|| "unknown".to_string());
    let thread = report.thread_name.as_deref().map_or_else(
        || "unknown".to_string(),
        |value| structured_field(value, 100),
    );
    let panic_type = report.panic_type.as_deref().map_or_else(
        || "unknown".to_string(),
        |value| structured_field(value, 50),
    );
    let template = report.panic_template.as_deref().map_or_else(
        || "unknown".to_string(),
        |value| structured_field(value, 50),
    );
    let message_len = report.panic_message_len.unwrap_or(0);

    // The message hash lives in its own column. Keeping it out of the summary
    // means the summary can be run through the scrubber (which would treat a
    // 64-character hex digest as a high-entropy secret) without corrupting it.
    format!(
        "{CRASH_SUMMARY_PREFIX} type={panic_type}; thread={thread}; location={location}; template={template}; message_len={message_len}; message=omitted; backtrace=omitted"
    )
}

/// Structured panic metadata extracted from a stored crash row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashMetadata {
    /// Panic payload type (`&str`, `String`, or `unknown`).
    pub panic_type: String,
    /// Name of the panicking thread, or `unknown`.
    pub thread_name: String,
    /// Panic source location (`file:line:column`), or `unknown`.
    pub panic_location: String,
}

impl CrashMetadata {
    /// Metadata recovered from a legacy free-form row: only the location is
    /// present, because the panic type and thread were never stored.
    fn legacy_from_text(text: &str) -> Self {
        Self {
            panic_type: "unknown".to_string(),
            thread_name: "unknown".to_string(),
            panic_location: panic_location_from_text(text).unwrap_or_else(|| "unknown".to_string()),
        }
    }
}

/// Parse the structured metadata out of a stored crash row.
///
/// A row written by the structured path carries [`CRASH_SUMMARY_PREFIX`]. A
/// legacy row ingested before structured capture was the default holds
/// free-form text, for which only the panic location can be recovered; the
/// free-form text itself is never returned.
pub fn crash_metadata_from_stored(stored: &str) -> CrashMetadata {
    static RE: OnceLock<regex::Regex> = OnceLock::new();

    if !stored.starts_with(CRASH_SUMMARY_PREFIX) {
        return CrashMetadata::legacy_from_text(stored);
    }

    let re = RE.get_or_init(|| {
        regex::Regex::new(r"type=([^;]*); thread=([^;]*); location=([^;]*)")
            .expect("structured crash summary regex is valid")
    });
    re.captures(stored).map_or_else(
        || CrashMetadata::legacy_from_text(stored),
        |caps| CrashMetadata {
            panic_type: structured_field(caps.get(1).map_or("", |m| m.as_str()), 50),
            thread_name: structured_field(caps.get(2).map_or("", |m| m.as_str()), 100),
            panic_location: structured_field(caps.get(3).map_or("", |m| m.as_str()), 200),
        },
    )
}

/// Whether a stored crash row already holds a structured summary.
pub fn is_structured(stored: &str) -> bool {
    stored.starts_with(CRASH_SUMMARY_PREFIX)
}

/// Recover the classification and symbol-only backtrace from a stored crash
/// row.
///
/// New rows carry the classification in dedicated columns. A legacy row (no
/// template column) is classified from its raw text on the fly; the text is
/// never returned and never uploaded by the default path.
pub fn stored_crash_view(
    crash: &crate::PendingCrash,
) -> (PanicClassification, Option<BacktraceSymbols>) {
    let classification = crash.panic_template.as_deref().map_or_else(
        || {
            if is_structured(&crash.panic_info) {
                // An old structured row has no message to classify or fingerprint.
                PanicClassification::empty()
            } else {
                classify_panic_message(panic_message_text(&crash.panic_info))
            }
        },
        |template| {
            let fields = crash
                .panic_fields
                .as_deref()
                .and_then(|json| {
                    serde_json::from_str::<BTreeMap<String, serde_json::Value>>(json).ok()
                })
                .map(sanitize_fields)
                .unwrap_or_default();
            PanicClassification {
                template: PanicTemplate::from_identifier(template),
                fields,
                message_len: usize::try_from(crash.panic_message_len.unwrap_or(0).max(0))
                    .unwrap_or(usize::MAX),
                message_sha256: safe_sha256(crash.panic_message_sha256.as_deref()),
            }
        },
    );

    let symbols = crash.backtrace_symbols.as_deref().map_or_else(
        || {
            crash
                .backtrace
                .as_deref()
                .map(symbolize_backtrace)
                .filter(|symbols| !symbols.frames.is_empty())
        },
        |json| {
            let frames = serde_json::from_str::<Vec<String>>(json).unwrap_or_default();
            rebuild_symbols(
                &frames,
                crash
                    .backtrace_frame_count
                    .and_then(|count| usize::try_from(count.max(0)).ok()),
            )
        },
    );

    (classification, symbols)
}

// ─── Local copies and pending files ─────────────────────────────────────────

/// Write a file readable only by the current user (`0600` on Unix).
///
/// Verbose pending files contain raw panic text, so the mode is not left to the
/// process umask. On non-Unix platforms this is a normal write; the platform's
/// own ACL model applies.
fn write_private_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Default maximum size for the pending crash directory (50 MB).
const DEFAULT_MAX_PENDING_BYTES: u64 = 50 * 1024 * 1024;

/// Whether `path` looks like a pending crash file.
fn is_pending_crash_file(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("json")
}

/// Remove pending crash files modified before `cutoff`.
///
/// A missing pending directory is not an error: it just means there is nothing
/// to prune yet.
fn prune_expired(pending_dir: &Path, cutoff: SystemTime) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(pending_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_pending_crash_file(&path) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if modified < cutoff && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Delete pending crash files older than `cutoff`, without a store or uploader.
///
/// This is safe to call on every startup — including the telemetry-disabled
/// path — because it only touches the local pending directory and records or
/// uploads nothing. Files modified at or after `cutoff` and non-`.json` entries
/// are left alone. A missing directory yields `Ok(0)`.
///
/// Typical use:
/// `prune_pending(&dir, SystemTime::now() - Duration::from_secs(DEFAULT_PENDING_MAX_AGE_SECS))`.
///
/// # Errors
///
/// Returns the directory-read error when the pending directory exists but
/// cannot be listed. Per-file metadata or deletion failures are skipped.
pub fn prune_pending(pending_dir: &Path, cutoff: SystemTime) -> std::io::Result<usize> {
    prune_expired(pending_dir, cutoff)
}

/// Crash reporter that hooks into the panic handler.
#[derive(Debug, Clone)]
pub struct CrashReporter {
    pending_dir: PathBuf,
    max_pending_bytes: u64,
    /// Strict (confidentiality) mode, resolved once at construction from
    /// [`crate::hipaa_enabled`] (`CODEREX_HIPAA`).
    strict: bool,
    /// Verbose mode, resolved once at construction from
    /// [`crash_verbose`] (`CODEREX_CRASH_VERBOSE`). Ignored when `strict` is
    /// set: strict mode always wins.
    verbose: bool,
}

impl CrashReporter {
    /// Create a new crash reporter.
    ///
    /// Structured-only reporting is the default. [`crash_verbose`]
    /// (`CODEREX_CRASH_VERBOSE`) opts into a raw local copy of the panic text
    /// (scrubbed at upload), and [`crate::hipaa_enabled`] (`CODEREX_HIPAA`)
    /// forces structured-only even when verbose is set.
    pub fn new(pending_dir: PathBuf) -> Self {
        Self {
            pending_dir,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
            strict: crate::hipaa_enabled(),
            verbose: crash_verbose(),
        }
    }

    /// Set the maximum allowed size of the pending crash directory.
    #[must_use]
    pub const fn with_max_pending_bytes(mut self, bytes: u64) -> Self {
        self.max_pending_bytes = bytes;
        self
    }

    /// Override strict mode explicitly.
    ///
    /// Production callers use [`Self::new`], which resolves `CODEREX_HIPAA`.
    /// Tests use this so they do not have to mutate process-wide environment
    /// variables.
    #[must_use]
    pub const fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Override verbose mode explicitly.
    ///
    /// Production callers use [`Self::new`], which resolves
    /// `CODEREX_CRASH_VERBOSE`. Tests use this so they do not have to mutate
    /// process-wide environment variables. Strict mode still wins.
    #[must_use]
    pub const fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Whether a raw local copy of the panic text may be written at all.
    ///
    /// The check happens where the panic payload exists (the hook) and again at
    /// ingestion, rather than only at upload.
    const fn include_free_form(&self) -> bool {
        self.verbose && !self.strict
    }

    /// Install a panic hook that records crashes to the pending directory.
    ///
    /// The original panic hook is preserved and called after recording.
    ///
    /// The pending file is written with mode `0600` on Unix. By default it
    /// contains only the classification and symbol-only backtrace; raw panic
    /// text and a raw backtrace are written only under
    /// `CODEREX_CRASH_VERBOSE` (and never under `CODEREX_HIPAA`).
    pub fn install_hook(&self) {
        let pending_dir = self.pending_dir.clone();
        let include_free_form = self.include_free_form();
        let strict = self.strict;
        let default_hook = std::panic::take_hook();

        std::panic::set_hook(Box::new(move |info| {
            // The host enables capture only after the telemetry settings have
            // resolved. While it is off, an installed hook is inert: run the
            // previous hook and write no pending file.
            if !capture_enabled() {
                default_hook(info);
                return;
            }

            let panic_type = if info.payload().is::<&str>() {
                "&str"
            } else if info.payload().is::<String>() {
                "String"
            } else {
                "unknown"
            };
            let panic_location = info.location().map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            });
            let thread_name = std::thread::current().name().map(str::to_string);
            let message = panic_payload_message(info);
            let classification = message
                .as_deref()
                .map_or_else(PanicClassification::empty, classify_panic_message);

            let mut report = CrashReport {
                panic_info: String::new(),
                backtrace: None,
                timestamp: chrono::Utc::now(),
                panic_location,
                thread_name,
                panic_type: Some(panic_type.to_string()),
                panic_template: Some(classification.template.as_str().to_string()),
                panic_fields: Some(
                    classification
                        .fields
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
                panic_message_len: Some(classification.message_len),
                panic_message_sha256: Some(classification.message_sha256),
                backtrace_symbols: None,
                backtrace_frame_count: None,
                backtrace_symbols_len: None,
                backtrace_symbols_sha256: None,
            };

            // Strict mode captures no backtrace at all. Otherwise the default
            // path captures it in order to reduce it to symbols; verbose also
            // keeps the raw text as a local copy.
            if !strict {
                let raw = format!("{:?}", std::backtrace::Backtrace::force_capture());
                let symbols = symbolize_backtrace(&raw);
                if !symbols.frames.is_empty() {
                    report.backtrace_symbols = Some(symbols.frames);
                    report.backtrace_frame_count = Some(symbols.frame_count);
                    report.backtrace_symbols_len = Some(symbols.symbols_len);
                    report.backtrace_symbols_sha256 = Some(symbols.symbols_sha256);
                }
                if include_free_form {
                    report.backtrace = Some(raw);
                }
            }

            if include_free_form {
                report.panic_info = format!("{info}");
            } else {
                report.panic_info = structured_summary(&report);
            }

            let _ = std::fs::create_dir_all(&pending_dir);
            let filename = format!("crash_{}.json", chrono::Utc::now().timestamp_millis());
            let path = pending_dir.join(filename);
            if let Ok(json) = serde_json::to_string(&report) {
                let _ = write_private_file(&path, json.as_bytes());
            }

            default_hook(info);
        }));
    }

    /// Scan the pending directory and ingest crash files.
    ///
    /// Before ingestion, pending files older than
    /// [`DEFAULT_PENDING_MAX_AGE_SECS`] are deleted, then — if the directory is
    /// still over `max_pending_bytes` — the oldest files are deleted until it
    /// fits. Stored crash rows older than [`DEFAULT_CRASH_RETENTION_DAYS`] are
    /// pruned as well.
    ///
    /// Returns the number of crashes ingested.
    pub fn ingest_pending(
        &self,
        store: &crate::TelemetryStore,
    ) -> Result<usize, crate::TelemetryError> {
        self.enforce_limits();
        let _ = store.prune_crashes(chrono::Duration::days(DEFAULT_CRASH_RETENTION_DAYS));

        let scrubber = crate::Scrubber::new();
        let mut count = 0;
        // The directory may not exist yet.
        let Ok(entries) = std::fs::read_dir(&self.pending_dir) else {
            return Ok(0);
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(report) = serde_json::from_str::<CrashReport>(&content)
            {
                let record = self.crash_record(&report, &scrubber);
                if store.insert_crash_record(&record).is_ok() {
                    let _ = std::fs::remove_file(&path);
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    /// Build the upload-row shape for a captured report without ingesting it.
    ///
    /// The modes resolved at construction ([`CODEREX_CRASH_VERBOSE`] and
    /// [`CODEREX_HIPAA`](crate::HIPAA_ENV)) are applied exactly as
    /// [`Self::ingest_pending`] would apply them: raw text is kept only when
    /// verbose is on and strict mode is off, symbol frames are dropped in
    /// strict mode, and the structured summary is scrubbed. The returned row
    /// has `id` 0 (it has no database row) and carries the report's own
    /// timestamp.
    ///
    /// This is the preview path for a pending file: pass the result to
    /// [`crate::preview_crash_payload`].
    ///
    /// [`CODEREX_CRASH_VERBOSE`]: crate::CRASH_VERBOSE_ENV
    #[must_use]
    pub fn pending_crash_from_report(&self, report: &CrashReport) -> crate::PendingCrash {
        let record = self.crash_record(report, &crate::Scrubber::new());
        crate::PendingCrash {
            id: 0,
            panic_info: record.panic_info,
            backtrace: record.backtrace,
            created_at: report.timestamp.to_rfc3339(),
            panic_template: record.panic_template,
            panic_fields: record.panic_fields,
            panic_message_len: record.panic_message_len,
            panic_message_sha256: record.panic_message_sha256,
            backtrace_symbols: record.backtrace_symbols,
            backtrace_frame_count: record.backtrace_frame_count,
            backtrace_symbols_len: record.backtrace_symbols_len,
            backtrace_symbols_sha256: record.backtrace_symbols_sha256,
        }
    }

    /// Turn a captured report into a row, respecting the current mode.
    fn crash_record(&self, report: &CrashReport, scrubber: &crate::Scrubber) -> CrashRecord {
        let classification = classification_from_report(report);
        let include_free_form = self.include_free_form();
        let has_raw_text = !is_structured(&report.panic_info);

        // The raw text is kept only when verbose was enabled at *both* capture
        // and ingestion, and never under strict mode. Otherwise the row holds a
        // structured summary and the free-form text is dropped here.
        let panic_info = if has_raw_text {
            if include_free_form {
                report.panic_info.clone()
            } else {
                let mut effective = report.clone();
                effective.panic_template = Some(classification.template.as_str().to_string());
                effective.panic_message_len = Some(classification.message_len);
                effective.panic_message_sha256 = Some(classification.message_sha256.clone());
                scrubber.scrub(&structured_summary(&effective))
            }
        } else {
            scrubber.scrub(&report.panic_info)
        };

        // Symbol-only frames are safe to keep in every mode except strict; the
        // raw backtrace is a verbose-only local copy.
        let symbols = if self.strict {
            None
        } else {
            symbols_from_report(report)
        };
        let backtrace = if include_free_form {
            report.backtrace.clone()
        } else {
            None
        };

        CrashRecord {
            panic_info,
            backtrace,
            panic_template: Some(classification.template.as_str().to_string()),
            panic_fields: serde_json::to_string(&classification.fields).ok(),
            panic_message_len: Some(i64::try_from(classification.message_len).unwrap_or(i64::MAX)),
            panic_message_sha256: Some(classification.message_sha256),
            backtrace_symbols: symbols
                .as_ref()
                .and_then(|symbols| serde_json::to_string(&symbols.frames).ok()),
            backtrace_frame_count: symbols
                .as_ref()
                .map(|symbols| i64::try_from(symbols.frame_count).unwrap_or(i64::MAX)),
            backtrace_symbols_len: symbols
                .as_ref()
                .map(|symbols| i64::try_from(symbols.symbols_len).unwrap_or(i64::MAX)),
            backtrace_symbols_sha256: symbols.map(|symbols| symbols.symbols_sha256),
        }
    }

    /// Delete pending files past their TTL, then enforce the byte budget.
    fn enforce_limits(&self) {
        let now = SystemTime::now();
        let ttl = std::time::Duration::from_secs(DEFAULT_PENDING_MAX_AGE_SECS);
        let cutoff = now.checked_sub(ttl).unwrap_or(SystemTime::UNIX_EPOCH);

        // TTL first: an old raw local copy should go even when there is plenty
        // of disk space. This is the same pruning logic exposed by
        // [`prune_pending`], so the disabled-startup path and ingestion stay in
        // sync.
        let _ = prune_pending(&self.pending_dir, cutoff);

        let mut files: Vec<(std::path::PathBuf, u64, SystemTime)> =
            match std::fs::read_dir(&self.pending_dir) {
                Ok(entries) => entries
                    .flatten()
                    .filter_map(|entry| {
                        let path = entry.path();
                        if !is_pending_crash_file(&path) {
                            return None;
                        }
                        let metadata = entry.metadata().ok()?;
                        let modified = metadata.modified().ok()?;
                        Some((path, metadata.len(), modified))
                    })
                    .collect(),
                Err(_) => return,
            };

        let total_size: u64 = files.iter().map(|(_, size, _)| size).sum();
        if total_size <= self.max_pending_bytes {
            return;
        }

        // Sort by mtime ascending (oldest first).
        files.sort_by_key(|a| a.2);

        let mut to_free = total_size - self.max_pending_bytes;
        for (path, size, _) in files {
            if to_free == 0 {
                break;
            }
            let _ = std::fs::remove_file(&path);
            to_free = to_free.saturating_sub(size);
        }
    }
}

/// Field keys the taxonomy is allowed to emit.
const SAFE_FIELD_KEYS: [&str; 7] = [
    "panic_index",
    "panic_container_len",
    "panic_range_start",
    "panic_range_end",
    "panic_slice_len",
    "panic_operation",
    "panic_error_type",
];

/// Arithmetic operation identifiers the classifier emits.
const SAFE_OPERATIONS: [&str; 10] = [
    "add",
    "sub",
    "mul",
    "div_overflow",
    "div_zero",
    "rem_overflow",
    "rem_zero",
    "neg",
    "shift_left",
    "shift_right",
];

/// Accept only a lowercase SHA-256 hex digest; anything else is discarded.
fn safe_sha256(value: Option<&str>) -> String {
    value
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .chars()
                    .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
        })
        .unwrap_or_default()
        .to_string()
}

fn is_safe_type_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.starts_with(|ch: char| ch.is_ascii_uppercase())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ':')
}

/// Validate a field recovered from a pending file or database row.
///
/// Pending files and the SQLite store are local, but they are still inputs: a
/// corrupt or hostile `panic_fields` blob must not smuggle free-form text into
/// an uploaded payload. Only the fixed key set and numeric/identifier value
/// shapes the classifier produces are accepted.
fn sanitize_field(key: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
    match key {
        "panic_index"
        | "panic_container_len"
        | "panic_range_start"
        | "panic_range_end"
        | "panic_slice_len" => value.as_u64().map(|number| serde_json::json!(number)),
        "panic_operation" => value
            .as_str()
            .filter(|operation| SAFE_OPERATIONS.contains(operation))
            .map(|operation| serde_json::json!(operation)),
        "panic_error_type" => value
            .as_str()
            .filter(|name| is_safe_type_name(name))
            .map(|name| serde_json::json!(name)),
        _ => None,
    }
}

fn sanitize_fields(
    fields: impl IntoIterator<Item = (String, serde_json::Value)>,
) -> BTreeMap<String, serde_json::Value> {
    fields
        .into_iter()
        .filter(|(key, _)| SAFE_FIELD_KEYS.contains(&key.as_str()))
        .filter_map(|(key, value)| sanitize_field(&key, &value).map(|value| (key, value)))
        .collect()
}

/// Rebuild a symbol list from stored input, re-sanitizing every frame.
fn rebuild_symbols(frames: &[String], frame_count: Option<usize>) -> Option<BacktraceSymbols> {
    let frames: Vec<String> = frames
        .iter()
        .filter_map(|frame| sanitize_symbol(frame))
        .take(MAX_BACKTRACE_FRAMES)
        .collect();
    if frames.is_empty() {
        return None;
    }
    let joined = frames.join("\n");
    let total = frame_count.unwrap_or(frames.len()).max(frames.len());
    Some(BacktraceSymbols {
        symbols_len: joined.len(),
        symbols_sha256: sha256_hex(joined.as_bytes()),
        frames,
        frame_count: total,
    })
}

/// Extract the panic payload message, if it is a string.
fn panic_payload_message(info: &std::panic::PanicHookInfo<'_>) -> Option<String> {
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        return Some((*message).to_string());
    }
    if let Some(message) = info.payload().downcast_ref::<String>() {
        return Some(message.clone());
    }
    None
}

/// Recover the classification from a captured (possibly legacy) report.
fn classification_from_report(report: &CrashReport) -> PanicClassification {
    if let Some(template) = report.panic_template.as_deref() {
        return PanicClassification {
            template: PanicTemplate::from_identifier(template),
            fields: sanitize_fields(report.panic_fields.clone().unwrap_or_default()),
            message_len: report.panic_message_len.unwrap_or(0),
            message_sha256: safe_sha256(report.panic_message_sha256.as_deref()),
        };
    }
    if is_structured(&report.panic_info) {
        return PanicClassification::empty();
    }
    classify_panic_message(panic_message_text(&report.panic_info))
}

/// Recover symbol-only frames from a captured report, symbolizing a legacy raw
/// backtrace when the report predates symbol capture.
fn symbols_from_report(report: &CrashReport) -> Option<BacktraceSymbols> {
    if let Some(frames) = report.backtrace_symbols.as_deref()
        && let Some(symbols) = rebuild_symbols(frames, report.backtrace_frame_count)
    {
        return Some(symbols);
    }
    report
        .backtrace
        .as_deref()
        .map(symbolize_backtrace)
        .filter(|symbols| !symbols.frames.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PendingCrash;

    fn crash_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("coderex_crash_{}_{}", std::process::id(), name))
    }

    fn pending_json_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| is_pending_crash_file(&entry.path()))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Panic hooks are process-global; serialize the tests that install one.
    static PANIC_HOOK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ingest_one(name: &str, report: &CrashReport, strict: bool, verbose: bool) -> PendingCrash {
        let dir = crash_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("crash_1.json"),
            serde_json::to_string(report).unwrap(),
        )
        .unwrap();

        let reporter = CrashReporter::new(dir.clone())
            .with_strict(strict)
            .with_verbose(verbose);
        let store = crate::TelemetryStore::open(&dir.join("telemetry.db")).unwrap();
        assert_eq!(reporter.ingest_pending(&store).unwrap(), 1);

        let crashes = store.pending_crashes(10).unwrap();
        assert_eq!(crashes.len(), 1);
        let result = crashes[0].clone();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn ingest_raw(name: &str, raw: serde_json::Value, strict: bool, verbose: bool) -> PendingCrash {
        let dir = crash_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("crash_1.json"), raw.to_string()).unwrap();

        let reporter = CrashReporter::new(dir.clone())
            .with_strict(strict)
            .with_verbose(verbose);
        let store = crate::TelemetryStore::open(&dir.join("telemetry.db")).unwrap();
        assert_eq!(reporter.ingest_pending(&store).unwrap(), 1);

        let crashes = store.pending_crashes(10).unwrap();
        assert_eq!(crashes.len(), 1);
        let result = crashes[0].clone();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn report_with(panic_info: &str, backtrace: Option<&str>) -> CrashReport {
        CrashReport {
            panic_info: panic_info.into(),
            backtrace: backtrace.map(str::to_string),
            timestamp: chrono::Utc::now(),
            panic_location: None,
            thread_name: None,
            panic_type: None,
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

    fn classified_report(message: &str, symbols: Option<&str>) -> CrashReport {
        let classification = classify_panic_message(message);
        let mut report = report_with("", None);
        report.panic_type = Some("&str".into());
        report.thread_name = Some("main".into());
        report.panic_location = Some("src/main.rs:10:5".into());
        report.panic_template = Some(classification.template.as_str().into());
        report.panic_fields = Some(
            classification
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        report.panic_message_len = Some(classification.message_len);
        report.panic_message_sha256 = Some(classification.message_sha256);
        if let Some(raw) = symbols {
            let symbols = symbolize_backtrace(raw);
            if !symbols.frames.is_empty() {
                report.backtrace_symbols = Some(symbols.frames);
                report.backtrace_frame_count = Some(symbols.frame_count);
                report.backtrace_symbols_len = Some(symbols.symbols_len);
                report.backtrace_symbols_sha256 = Some(symbols.symbols_sha256);
            }
        }
        report.panic_info = structured_summary(&report);
        report
    }

    #[test]
    fn a_pending_file_can_be_previewed_with_the_ingestion_modes() {
        let report = classified_report("called `Option::unwrap()` on a `None` value", None);
        let reporter = CrashReporter::new(PathBuf::from("unused"))
            .with_strict(false)
            .with_verbose(false);
        let pending = reporter.pending_crash_from_report(&report);

        assert_eq!(pending.id, 0);
        assert_eq!(pending.created_at, report.timestamp.to_rfc3339());
        assert_eq!(
            pending.panic_template.as_deref(),
            Some(PanicTemplate::UnwrapNone.as_str())
        );
        assert!(pending.backtrace.is_none());

        // The default preview is the structured wire form: no free-form text.
        let preview = crate::preview_crash_payload_with(&pending, false, false);
        assert_eq!(
            preview["panic_template"],
            PanicTemplate::UnwrapNone.as_str()
        );
        assert!(preview.get("panic_info").is_none());

        // Verbose keeps the raw local copy and the preview exposes it scrubbed;
        // strict wins and reduces it back to the structured summary.
        let raw = report_with("panicked at src/main.rs:10:5:\\nboom", None);
        let verbose = CrashReporter::new(PathBuf::from("unused"))
            .with_strict(false)
            .with_verbose(true)
            .pending_crash_from_report(&raw);
        let preview = crate::preview_crash_payload_with(&verbose, true, false);
        assert!(preview["panic_info"].as_str().unwrap().contains("boom"));

        let strict = CrashReporter::new(PathBuf::from("unused"))
            .with_strict(true)
            .with_verbose(true)
            .pending_crash_from_report(&raw);
        let preview = crate::preview_crash_payload_with(&strict, true, true);
        assert!(preview.get("panic_info").is_none());
        assert!(is_structured(&strict.panic_info));
    }

    // ── Panic templates ─────────────────────────────────────────────────

    #[test]
    fn classifies_unwrap_and_expect_shapes() {
        let cases = [
            (
                "called `Result::unwrap()` on an `Err` value: ParseIntError { kind: InvalidDigit }",
                PanicTemplate::UnwrapErr,
            ),
            (
                "called `Result::unwrap()` on an `Err` value: PoisonError { .. }",
                PanicTemplate::PoisonedLock,
            ),
            (
                "called `Option::unwrap()` on a `None` value",
                PanicTemplate::UnwrapNone,
            ),
            (
                "called `Result::expect()` on an `Err` value: CustomError(\"x\")",
                PanicTemplate::ExpectErr,
            ),
            (
                "called `Option::expect()` on a `None` value",
                PanicTemplate::ExpectNone,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                classify_panic_message(message).template,
                expected,
                "{message}"
            );
        }
    }

    #[test]
    fn classifies_index_out_of_bounds_with_safe_fields() {
        let classification =
            classify_panic_message("index out of bounds: the len is 3 but the index is 7");
        assert_eq!(classification.template, PanicTemplate::IndexOutOfBounds);
        assert_eq!(
            classification.fields.get("panic_container_len"),
            Some(&serde_json::json!(3))
        );
        assert_eq!(
            classification.fields.get("panic_index"),
            Some(&serde_json::json!(7))
        );
    }

    #[test]
    fn classifies_slice_ranges_with_safe_fields() {
        let start =
            classify_panic_message("range start index 9 out of range for slice of length 4");
        assert_eq!(start.template, PanicTemplate::SliceRange);
        assert_eq!(
            start.fields.get("panic_range_start"),
            Some(&serde_json::json!(9))
        );
        assert_eq!(
            start.fields.get("panic_slice_len"),
            Some(&serde_json::json!(4))
        );

        let end = classify_panic_message("range end index 9 out of range for slice of length 4");
        assert_eq!(
            end.fields.get("panic_range_end"),
            Some(&serde_json::json!(9))
        );

        let both = classify_panic_message("slice index starts at 8 but ends at 2");
        assert_eq!(
            both.fields.get("panic_range_start"),
            Some(&serde_json::json!(8))
        );
        assert_eq!(
            both.fields.get("panic_range_end"),
            Some(&serde_json::json!(2))
        );
    }

    #[test]
    fn classifies_capacity_arithmetic_assertion_and_explicit() {
        let cases = [
            ("capacity overflow", PanicTemplate::CapacityOverflow),
            (
                "attempt to add with overflow",
                PanicTemplate::ArithmeticOverflow,
            ),
            (
                "attempt to divide by zero",
                PanicTemplate::ArithmeticOverflow,
            ),
            (
                "assertion failed: value > 0",
                PanicTemplate::AssertionFailed,
            ),
            (
                "assertion `left == right` failed\n  left: 1\n right: 2",
                PanicTemplate::AssertionFailed,
            ),
            ("explicit panic", PanicTemplate::ExplicitPanic),
        ];
        for (message, expected) in cases {
            assert_eq!(
                classify_panic_message(message).template,
                expected,
                "{message}"
            );
        }
        let arithmetic = classify_panic_message("attempt to multiply with overflow");
        assert_eq!(
            arithmetic.fields.get("panic_operation"),
            Some(&serde_json::json!("mul"))
        );
    }

    #[test]
    fn extracts_an_error_type_but_never_prose() {
        let classification = classify_panic_message(
            "called `Result::unwrap()` on an `Err` value: MyError(\"user said something\")",
        );
        assert_eq!(
            classification.fields.get("panic_error_type"),
            Some(&serde_json::json!("MyError"))
        );

        let prose = classify_panic_message(
            "called `Result::unwrap()` on an `Err` value: USER SAID SOMETHING PRIVATE",
        );
        assert!(!prose.fields.contains_key("panic_error_type"));
    }

    #[test]
    fn unknown_message_classification_carries_only_length_and_hash() {
        let message = "the user asked about a private topic and the reply was confidential";
        let classification = classify_panic_message(message);
        assert_eq!(classification.template, PanicTemplate::Unknown);
        assert!(classification.fields.is_empty());
        assert_eq!(classification.message_len, message.len());
        assert_eq!(classification.message_sha256.len(), 64);
        assert_eq!(
            classification.message_sha256,
            sha256_hex(message.as_bytes())
        );
    }

    // ── Backtrace symbols ───────────────────────────────────────────────

    const BACKTRACE: &str = "   0: std::panicking::begin_panic_handler\n             at /rustc/abcdef0123456789abcdef0123456789/library/std/src/panicking.rs:123:5\n   1: coderex_core::engine::QueryEngine::submit\n             at /home/somebody/project/crates/core/src/engine.rs:42:9\n   2: <unknown>\n";

    #[test]
    fn symbolization_drops_paths_usernames_and_source_lines() {
        let symbols = symbolize_backtrace(BACKTRACE);
        assert_eq!(
            symbols.frames,
            vec![
                "std::panicking::begin_panic_handler",
                "coderex_core::engine::QueryEngine::submit",
            ]
        );
        let rendered = symbols.frames.join("\n");
        for forbidden in ["/", "somebody", "/home", "/rustc", "engine.rs", ":42:9"] {
            assert!(!rendered.contains(forbidden), "{forbidden}: {rendered}");
        }
        assert_eq!(symbols.symbols_len, rendered.len());
        assert_eq!(symbols.symbols_sha256, sha256_hex(rendered.as_bytes()));
    }

    #[test]
    fn symbolization_keeps_at_most_the_top_n_frames() {
        let mut raw = String::new();
        for index in 0..(MAX_BACKTRACE_FRAMES + 6) {
            raw.push_str(&format!("   {index}: crate_{index}::function_{index}\n"));
        }
        let symbols = symbolize_backtrace(&raw);
        assert_eq!(symbols.frames.len(), MAX_BACKTRACE_FRAMES);
        assert_eq!(symbols.frame_count, MAX_BACKTRACE_FRAMES + 6);
        assert!(symbols.frames[0].contains("crate_0::"));
        assert!(
            !symbols
                .frames
                .iter()
                .any(|frame| frame.contains("crate_29::"))
        );
    }

    #[test]
    fn a_symbol_with_path_punctuation_is_dropped_rather_than_leaked() {
        let raw = "   0: /Users/somebody/project/bin\n   1: ok::symbol\n";
        let symbols = symbolize_backtrace(raw);
        assert_eq!(symbols.frames, vec!["ok::symbol"]);
    }

    // ── Ingestion modes ─────────────────────────────────────────────────

    /// The default reduces raw text and a raw backtrace to structured metadata
    /// and symbols; neither the message nor the path-bearing backtrace reaches
    /// the store.
    #[test]
    fn default_ingestion_stores_structured_and_symbolic_only() {
        let report = report_with(
            "panicked at /Users/somebody/project/src/main.rs:10:5:\nfailed for user@example.com from 10.0.0.7",
            Some(BACKTRACE),
        );
        let crash = ingest_one("structured_default", &report, false, false);

        assert!(is_structured(&crash.panic_info), "{}", crash.panic_info);
        assert!(crash.panic_info.contains("message=omitted"));
        assert!(crash.backtrace.is_none());
        assert_eq!(crash.panic_template.as_deref(), Some("unknown"));
        assert!(crash.panic_message_len.unwrap() > 0);
        assert_eq!(crash.panic_message_sha256.as_deref().unwrap().len(), 64);
        let symbols = crash.backtrace_symbols.unwrap();
        assert!(!symbols.contains("/"));
        assert!(!symbols.contains("somebody"));

        // The local summary may keep a (now user-redacted) absolute path; the
        // *upload* strips the root. What must never survive is the username or
        // the message itself.
        for forbidden in ["user@example.com", "10.0.0.7", "somebody", "failed for"] {
            assert!(
                !crash.panic_info.contains(forbidden),
                "{forbidden}: {}",
                crash.panic_info
            );
        }
    }

    /// Verbose keeps a raw local copy (so a user can inspect it) but the
    /// upload path is what scrubs; see uploader tests for the wire assertion.
    #[test]
    fn verbose_ingestion_keeps_a_raw_local_copy() {
        let raw = "panicked at src/main.rs:10:5:\nuser said: here is the key A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6";
        let report = report_with(raw, Some(BACKTRACE));
        let crash = ingest_one("verbose_raw_local", &report, false, true);

        assert!(crash.panic_info.contains("user said"));
        assert!(
            crash
                .panic_info
                .contains("A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6")
        );
        assert!(
            crash
                .backtrace
                .as_deref()
                .unwrap()
                .contains("/home/somebody")
        );
        // Even the local copy keeps symbolized frames, so an upload needs no
        // re-parse of the raw text.
        assert!(crash.backtrace_symbols.unwrap().contains("QueryEngine"));
    }

    /// `CODEREX_HIPAA` always wins: even with verbose enabled, ingestion stores
    /// a structured summary, no raw text and no backtrace.
    #[test]
    fn strict_wins_over_verbose() {
        let report = report_with(
            "panicked at src/main.rs:10:5:\nuser said something private",
            Some(BACKTRACE),
        );
        let crash = ingest_one("strict_over_verbose", &report, true, true);
        assert!(crash.backtrace.is_none(), "{:?}", crash.backtrace);
        assert!(is_structured(&crash.panic_info), "{}", crash.panic_info);
        assert!(crash.panic_info.contains("message=omitted"));
        assert!(!crash.panic_info.contains("user said"));
        assert!(crash.backtrace_symbols.is_none());
    }

    #[test]
    fn verbose_env_values_parse() {
        assert!(crash_verbose_with(Some("1")));
        assert!(crash_verbose_with(Some("true")));
        assert!(crash_verbose_with(Some("YES")));
        assert!(!crash_verbose_with(Some("0")));
        assert!(!crash_verbose_with(Some("false")));
        assert!(!crash_verbose_with(Some("no")));
        assert!(!crash_verbose_with(Some("")));
        assert!(!crash_verbose_with(None));
        assert_eq!(CRASH_VERBOSE_ENV, "CODEREX_CRASH_VERBOSE");
    }

    /// A classified report's summary carries the template and fingerprint but
    /// no message text.
    #[test]
    fn structured_summary_carries_template_and_fingerprint_only() {
        let report = classified_report(
            "index out of bounds: the len is 3 but the index is 7",
            Some(BACKTRACE),
        );
        assert!(report.panic_info.starts_with(CRASH_SUMMARY_PREFIX));
        assert!(report.panic_info.contains("template=index_out_of_bounds"));
        assert!(report.panic_info.contains("message_len="));
        assert!(!report.panic_info.contains("index is 7"));
        assert!(report.panic_info.contains("message=omitted"));
        assert!(report.panic_info.contains("backtrace=omitted"));
    }

    #[test]
    fn a_structured_report_is_ingested_without_free_form() {
        let report = classified_report(
            "index out of bounds: the len is 3 but the index is 7",
            Some(BACKTRACE),
        );
        let crash = ingest_one("classified_report", &report, false, false);
        assert_eq!(crash.panic_template.as_deref(), Some("index_out_of_bounds"));
        assert_eq!(
            crash.panic_fields.as_deref(),
            Some(r#"{"panic_container_len":3,"panic_index":7}"#)
        );
        assert!(crash.backtrace_symbols.is_some());
        assert!(crash.backtrace.is_none());
    }

    // ── Legacy rows and files ───────────────────────────────────────────

    fn legacy_raw_report() -> serde_json::Value {
        serde_json::json!({
            "panic_info": "panicked at src/old.rs:7:2:\nuser said something private",
            "backtrace": BACKTRACE,
            "timestamp": "2026-01-01T00:00:00Z"
        })
    }

    /// A legacy pending file in the default mode is reduced to a structured
    /// summary, classified from the text, and its raw backtrace becomes
    /// symbols. The free-form text is never stored or uploaded.
    #[test]
    fn default_mode_reduces_legacy_free_form_pending_file() {
        let crash = ingest_raw("legacy_default", legacy_raw_report(), false, false);
        assert!(is_structured(&crash.panic_info), "{}", crash.panic_info);
        assert!(crash.panic_info.contains("location=src/old.rs:7:2"));
        assert!(crash.panic_info.contains("message=omitted"));
        assert!(!crash.panic_info.contains("user said"));
        assert!(crash.backtrace.is_none());
        assert!(
            crash
                .backtrace_symbols
                .as_deref()
                .unwrap()
                .contains("QueryEngine")
        );
        assert_eq!(crash.panic_template.as_deref(), Some("unknown"));
        assert!(crash.panic_message_len.unwrap() > 0);
    }

    /// Strict mode behaves the same for a legacy file, even with verbose
    /// enabled, and stores no symbols either.
    #[test]
    fn strict_mode_handles_legacy_free_form_pending_file() {
        let crash = ingest_raw("legacy_strict", legacy_raw_report(), true, true);
        assert!(is_structured(&crash.panic_info), "{}", crash.panic_info);
        assert!(crash.panic_info.contains("location=src/old.rs:7:2"));
        assert!(!crash.panic_info.contains("user said"));
        assert!(crash.backtrace.is_none());
        assert!(crash.backtrace_symbols.is_none());
    }

    /// Verbose mode is the only path that keeps a legacy file's free-form text,
    /// and it is a local copy.
    #[test]
    fn verbose_mode_ingests_legacy_text_as_a_local_copy() {
        let crash = ingest_raw("legacy_verbose", legacy_raw_report(), false, true);
        assert!(crash.panic_info.contains("user said"));
        assert!(
            crash
                .backtrace
                .as_deref()
                .unwrap()
                .contains("/home/somebody")
        );
    }

    /// A summary written by the default hook survives a later verbose
    /// ingestion unchanged: there is no free-form text to recover.
    #[test]
    fn verbose_ingestion_passes_through_a_structured_pending_file() {
        let report = classified_report("explicit panic", None);
        let crash = ingest_one("structured_verbose", &report, false, true);
        assert!(is_structured(&crash.panic_info), "{}", crash.panic_info);
        assert!(crash.panic_info.contains("location=src/main.rs:10:5"));
        assert!(crash.backtrace.is_none());
    }

    // ── Local copy permissions and TTL ─────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn pending_file_is_written_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crash_dir("mode_0600");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crash_1.json");
        write_private_file(&path, b"{}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "raw local copy must be user-only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_files_past_ttl_are_pruned_without_ingestion() {
        let dir = crash_dir("ttl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crash_old.json");
        std::fs::write(&path, "{}").unwrap();

        // Age the file past the TTL.
        let old = std::time::SystemTime::now()
            - std::time::Duration::from_secs(DEFAULT_PENDING_MAX_AGE_SECS + 60);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(old).unwrap();
        drop(file);

        let reporter = CrashReporter::new(dir.clone());
        let store = crate::TelemetryStore::open(&dir.join("telemetry.db")).unwrap();
        assert_eq!(reporter.ingest_pending(&store).unwrap(), 0);
        assert!(!path.exists(), "an expired raw local copy must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The host installs the hook unconditionally, but an opted-out session
    /// leaves capture disabled: a panic must write no pending file.
    #[test]
    fn capture_disabled_writes_no_pending_file() {
        let _guard = PANIC_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = crash_dir("capture_disabled");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let previous = std::panic::take_hook();
        set_capture_enabled(false);
        CrashReporter::new(dir.clone())
            .with_strict(false)
            .with_verbose(false)
            .install_hook();

        let result = std::panic::catch_unwind(|| panic!("capture-disabled test panic"));
        assert!(result.is_err());

        std::panic::set_hook(previous);
        assert_eq!(
            pending_json_files(&dir),
            0,
            "capture is disabled, so the panic must not leave a pending file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_enabled_writes_a_pending_file() {
        let _guard = PANIC_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = crash_dir("capture_enabled");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let previous = std::panic::take_hook();
        set_capture_enabled(true);
        CrashReporter::new(dir.clone())
            .with_strict(false)
            .with_verbose(false)
            .install_hook();

        let result = std::panic::catch_unwind(|| panic!("capture-enabled test panic"));
        assert!(result.is_err());

        std::panic::set_hook(previous);
        set_capture_enabled(false);
        assert_eq!(
            pending_json_files(&dir),
            1,
            "an enabled capture must write exactly one pending file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pruning is available to the telemetry-disabled startup path: it needs
    /// only the directory and a cutoff, and removes exactly the expired files.
    #[test]
    fn prune_pending_removes_only_files_older_than_the_cutoff() {
        let dir = crash_dir("prune_explicit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let now = SystemTime::now();
        let ttl = std::time::Duration::from_secs(DEFAULT_PENDING_MAX_AGE_SECS);
        let old = dir.join("crash_old.json");
        let fresh = dir.join("crash_fresh.json");
        let not_a_crash = dir.join("notes.txt");
        for path in [&old, &fresh, &not_a_crash] {
            std::fs::write(path, "{}").unwrap();
        }
        let day = std::time::Duration::from_secs(24 * 60 * 60);
        let file = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
        file.set_modified(now - 10 * day).unwrap();
        drop(file);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&fresh)
            .unwrap();
        file.set_modified(now - day).unwrap();
        drop(file);

        let cutoff = now.checked_sub(ttl).unwrap();
        assert_eq!(prune_pending(&dir, cutoff).unwrap(), 1);

        assert!(!old.exists(), "a file past the TTL must be removed");
        assert!(fresh.exists(), "a file within the TTL must be kept");
        assert!(not_a_crash.exists(), "non-crash files must be ignored");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_pending_missing_directory_is_not_an_error() {
        let dir = crash_dir("prune_missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            prune_pending(&dir, SystemTime::now()).unwrap(),
            0,
            "a missing pending directory has nothing to prune"
        );
    }

    #[test]
    fn crash_report_serialization_round_trips_new_fields() {
        let report = classified_report(
            "index out of bounds: the len is 3 but the index is 7",
            Some(BACKTRACE),
        );
        let json = serde_json::to_string(&report).unwrap();
        let decoded: CrashReport = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.panic_template.as_deref(),
            Some("index_out_of_bounds")
        );
        assert_eq!(decoded.panic_message_len, report.panic_message_len);
        assert_eq!(decoded.backtrace_symbols, report.backtrace_symbols);
        assert!(!json.contains("index is 7"));
    }
}
