//! PII scrubbing for telemetry events.
//!
//! Sanitizes sensitive data from event payloads before storage.
//! Covers: email addresses, IP addresses, file paths containing usernames,
//! API keys and other credential shapes, long high-entropy tokens, an
//! exclusion set of field names that are dropped from payloads entirely, and
//! model-name redaction (a dedicated key regex plus an exclusion set). A
//! stricter mode — enabled for confidentiality-sensitive deployments —
//! additionally validates a payload against a field allowlist and marks
//! anything it cannot validate.
//!
//! The scrubber removes *patterns*, not *meaning*. A sentence copied from a
//! conversation is left in the clear unless it happens to contain one of the
//! patterns above. Crash reporting accounts for this explicitly: under strict
//! mode it never ships free-form panic text. See `crash.rs` and
//! `docs/telemetry.md`.

use regex::Regex;
use std::sync::OnceLock;

/// JSON object keys that must never appear in an uploaded payload,
/// regardless of which event carried them (case-insensitive).
pub const EXCLUDED_FIELDS: [&str; 8] = [
    "api_key",
    "apikey",
    "authorization",
    "password",
    "secret",
    "token",
    "cookie",
    "set-cookie",
];

/// Whether a JSON object key is on the never-send list.
fn is_excluded_field(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    EXCLUDED_FIELDS.contains(&lower.as_str())
}

// ─── Model-name redaction ───────────────────────────────────────────────────

/// Object keys always treated as model-name fields, whether or not they match
/// the model-key regex.
pub const ALWAYS_MODEL_KEYS: [&str; 3] = ["chain_entry", "fallbackFrom", "shortName"];

/// Object keys the model-key regex would match but which are explicitly *not*
/// model-name fields.
pub const EXCLUDED_MODEL_KEYS: [&str; 1] = ["repository_model"];

/// Values that name a non-model placeholder rather than a concrete model.
///
/// A model field holding one of these is left exactly as-is: it identifies no
/// model, so there is nothing to hide.
pub const MODEL_PLACEHOLDERS: [&str; 5] =
    ["inherit", "default", "none", "unknown", "nonconforming"];

/// The constant substituted for every confidential (or unavailable) model name.
pub const CONFIDENTIAL_MODEL: &str = "confidential";

/// The compiled model-key regex (the reference's `xN` pattern).
fn model_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:model|model_id|[A-Za-z0-9_]*_models?|[A-Za-z0-9_]*[a-z0-9]Models?)$")
            .expect("model-key regex is valid")
    })
}

/// Whether a JSON object key names a model.
///
/// A key is a model field when it is one of [`ALWAYS_MODEL_KEYS`], or — when it
/// is not one of [`EXCLUDED_MODEL_KEYS`] — it matches the model-key regex
/// (`model`, `model_id`, `*_model(s)`, `*Model(s)`). The exclusion is applied
/// before the regex so `repository_model`, which the regex would otherwise
/// match, is never treated as a model field.
#[must_use]
pub fn is_model_key(key: &str) -> bool {
    if ALWAYS_MODEL_KEYS.contains(&key) {
        return true;
    }
    if EXCLUDED_MODEL_KEYS.contains(&key) {
        return false;
    }
    model_key_regex().is_match(key)
}

/// The value a model field should carry after redaction.
///
/// A placeholder is kept verbatim. Anything else — a concrete model id, or an
/// unavailable value (empty string, or not a string at all) — becomes
/// [`CONFIDENTIAL_MODEL`].
///
/// CodeRex has a single first-party channel and no cloud model catalogue, so it
/// cannot know which names are sensitive: every concrete name is treated as
/// confidential, and only the placeholders (which name no model) are left in
/// the clear. See `docs/telemetry.md`.
fn redacted_model_value(current: &serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::String(s) = current {
        if MODEL_PLACEHOLDERS.contains(&s.as_str()) {
            return current.clone();
        }
    }
    serde_json::Value::String(CONFIDENTIAL_MODEL.to_string())
}

/// Replace every model-name value in `value`, recursively.
///
/// Every object key for which [`is_model_key`] holds has its value replaced
/// with [`CONFIDENTIAL_MODEL`] (placeholders preserved). A model field's value
/// is replaced whole, so a subtree hanging off a model key is not walked.
pub fn redact_model_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_model_key(key) {
                    *child = redacted_model_value(child);
                } else {
                    redact_model_fields(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                redact_model_fields(item);
            }
        }
        _ => {}
    }
}

// ─── Strict (confidentiality) mode ──────────────────────────────────────────

/// Field names a telemetry payload may carry under strict mode.
///
/// The union of the event schema fields and the crash schema fields: strict
/// mode sends a payload only if every key at every level is one of these.
/// `redaction_status` is allowed so a payload that already carries an explicit
/// redaction mark (for example a structured crash summary) validates instead
/// of having its mark overwritten with `unredacted`.
pub const STRICT_ALLOWED_FIELDS: [&str; 34] = [
    "type",
    "session_id",
    "tool_name",
    "success",
    "duration_ms",
    "provider",
    "model",
    "tokens_in",
    "tokens_out",
    "error_type",
    "message",
    "timestamp",
    "panic_info",
    "backtrace",
    "redaction_status",
    // Structured crash metadata (the default crash payload).
    "panic_type",
    "thread_name",
    "panic_location",
    "os",
    "version",
    // Panic taxonomy: the template identity, length/hash for unknown messages,
    // and the fixed safe parsed fields.
    "panic_template",
    "panic_message_len",
    "panic_message_sha256",
    "panic_index",
    "panic_container_len",
    "panic_range_start",
    "panic_range_end",
    "panic_slice_len",
    "panic_operation",
    "panic_error_type",
    // Symbol-only backtrace (no paths or source text).
    "backtrace_frames",
    "backtrace_frame_count",
    "backtrace_symbols_len",
    "backtrace_symbols_sha256",
];

/// Key inserted into a payload that strict mode could not validate.
pub const UNREDACTED_MARKER_KEY: &str = "redaction_status";

/// Value written under [`UNREDACTED_MARKER_KEY`] for an unvalidated payload.
pub const UNREDACTED_MARKER_VALUE: &str = "unredacted";

/// Whether every object key in `value` is on [`STRICT_ALLOWED_FIELDS`].
///
/// Walks the whole tree: a payload is validated only when every key at every
/// level is known. Arrays are traversed; scalars are trivially allowed.
#[must_use]
pub fn strictly_allowlisted(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().all(|(key, child)| {
            STRICT_ALLOWED_FIELDS.contains(&key.as_str()) && strictly_allowlisted(child)
        }),
        serde_json::Value::Array(items) => items.iter().all(strictly_allowlisted),
        _ => true,
    }
}

/// Mark `value` as unredacted when it cannot be validated in strict mode.
///
/// Returns `true` when the marker was written. A payload that fails the
/// allowlist is still sent — dropping it would lose telemetry silently, and
/// sending it unmarked would claim a validation that did not happen — so it
/// carries [`UNREDACTED_MARKER_KEY`] instead. The marker travels in the payload
/// itself, so it is visible in the stored and uploaded event rather than only
/// in a log line.
pub fn mark_unredacted_if_unallowlisted(value: &mut serde_json::Value) -> bool {
    if strictly_allowlisted(value) {
        return false;
    }
    match value {
        serde_json::Value::Object(map) => {
            map.insert(
                UNREDACTED_MARKER_KEY.to_string(),
                serde_json::Value::String(UNREDACTED_MARKER_VALUE.to_string()),
            );
            true
        }
        _ => false,
    }
}

/// Minimum length of a candidate high-entropy token.
///
/// 32 characters is the shortest common digest / key length (MD5 is 32 hex
/// characters, SHA-256 is 64); shorter runs collide with ordinary identifiers.
pub const HIGH_ENTROPY_MIN_LEN: usize = 32;

/// Replacement written over a detected high-entropy secret.
pub const SECRET_PLACEHOLDER: &str = "<SECRET>";

/// Minimum Shannon entropy, in bits per character, for a non-hex candidate.
///
/// Random base64/mixed-alphanumeric secrets sit around 5–6 bits/char; a
/// repeated or structured identifier sits below 3. Hex digests top out at 4
/// bits/char and random ones sit near 3.9.
const MIN_TOKEN_ENTROPY_BITS: f64 = 3.0;

/// Candidate runs: base64url/hex/mixed-alphanumeric characters.
///
/// `/`, `.` and `:` are deliberately excluded so paths, URLs and symbol paths
/// (`module::function`) are split into short segments rather than matched as a
/// single token.
fn high_entropy_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9_+=\-]{32,}").expect("high-entropy regex is valid"))
}

/// Shannon entropy per character over the token's bytes.
///
/// The counts are token lengths (at most a few kilobytes), so the `usize` to
/// `f64` conversion is exact in practice; the lint guards a range this code
/// never reaches.
#[allow(clippy::cast_precision_loss)]
fn shannon_entropy(token: &str) -> f64 {
    let mut counts = [0usize; 256];
    let mut total = 0usize;
    for byte in token.bytes() {
        counts[byte as usize] += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }
    let total = total as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = count as f64 / total;
            -p * p.log2()
        })
        .sum()
}

/// Whether the run at `text[start..end]` is a secret-like high-entropy token.
///
/// Conservative by design: a false positive mangles a file path or symbol that
/// diagnostics depend on, so ambiguous runs are left alone. The checks are:
///
/// - at least [`HIGH_ENTROPY_MIN_LEN`] characters;
/// - not part of a path or URL (adjacent `/`, `\`, `.`, `@`, `~`) and not the
///   segment of a Rust symbol path (adjacent `::`);
/// - not a Rust mangled symbol (`_ZN…` legacy / `_R…` v0);
/// - all-hex runs of that length are accepted (digests);
/// - otherwise the run must mix uppercase, lowercase and digits, and have
///   entropy of at least [`MIN_TOKEN_ENTROPY_BITS`]. All-lowercase
///   (`snake_case`) and all-uppercase (`CONSTANT`) identifiers are rejected.
fn is_high_entropy_token(text: &str, start: usize, end: usize) -> bool {
    let token = &text[start..end];
    if token.len() < HIGH_ENTROPY_MIN_LEN {
        return false;
    }

    // Neighbouring path / URL / symbol punctuation means this is a component
    // of something structured, not a standalone secret.
    let prev = text[..start].chars().next_back();
    let next = text[end..].chars().next();
    if matches!(prev, Some('/' | '\\' | '.' | '@' | '~'))
        || matches!(next, Some('/' | '\\' | '.' | '@' | '~'))
        || text[..start].ends_with("::")
        || text[end..].starts_with("::")
    {
        return false;
    }

    // Rust mangled symbols are long, mixed, and not secrets.
    if token.starts_with("_ZN") || token.starts_with("_R") {
        return false;
    }

    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());

    // Pure hex of digest length (MD5/SHA/HMAC) is a secret shape even though it
    // uses only two character classes.
    if token.chars().all(|c| c.is_ascii_hexdigit()) {
        return shannon_entropy(token) >= MIN_TOKEN_ENTROPY_BITS;
    }

    // Everything else must genuinely mix character classes: letters in both
    // cases plus digits. That excludes snake_case, kebab-case, CamelCase and
    // CONSTANT identifiers, which are the shapes backtraces and prose contain.
    has_lower && has_upper && has_digit && shannon_entropy(token) >= MIN_TOKEN_ENTROPY_BITS
}

/// Replace detected high-entropy tokens with [`SECRET_PLACEHOLDER`].
fn scrub_high_entropy_tokens(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for candidate in high_entropy_regex().find_iter(text) {
        if !is_high_entropy_token(text, candidate.start(), candidate.end()) {
            continue;
        }
        result.push_str(&text[last_end..candidate.start()]);
        result.push_str(SECRET_PLACEHOLDER);
        last_end = candidate.end();
    }
    result.push_str(&text[last_end..]);
    result
}

// ─── Numeric / financial detectors ──────────────────────────────────────────

/// Replacement written over a Luhn-valid payment card number.
pub const CREDIT_CARD_PLACEHOLDER: &str = "<CREDIT_CARD>";

/// Replacement written over a US Social Security number.
pub const SSN_PLACEHOLDER: &str = "<SSN>";

/// Replacement written over a checksum-valid IBAN.
pub const IBAN_PLACEHOLDER: &str = "<IBAN>";

/// Whether a match is embedded in a path, symbol or `file:line` frame.
///
/// The numeric detectors are shape-only, so a card-like run inside a path or a
/// line number must be left alone. A trailing `.` is treated as an extension
/// only when an identifier character follows it; otherwise it is sentence
/// punctuation.
fn is_embedded_in_structured_value(text: &str, start: usize, end: usize) -> bool {
    let path_char = |c: char| matches!(c, '/' | '\\' | '~' | '_' | '-');
    if text[..start]
        .chars()
        .next_back()
        .is_some_and(|c| path_char(c) || c == ':')
    {
        return true;
    }
    let next = text[end..].chars().next();
    if next.is_some_and(|c| path_char(c) || c == ':') {
        return true;
    }
    if next == Some('.') {
        let after_dot = text[end + 1..].chars().next();
        if after_dot.is_some_and(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
    }
    false
}

/// Luhn checksum over an ASCII digit string.
fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for byte in digits.bytes().rev() {
        let mut value = u32::from(byte.saturating_sub(b'0'));
        if double {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
        double = !double;
    }
    sum % 10 == 0
}

/// Replace payment card numbers that pass the Luhn checksum.
///
/// Conservative by design: 13–19 digits, no consecutive separators, and never
/// a run embedded in a path (`/data/4111111111111111/report`), a filename
/// (`card-4111111111111111.txt`) or a `file:line` frame. A number that fails
/// Luhn is not a card and is left alone.
fn scrub_credit_cards(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\b[0-9][0-9 -]{10,25}[0-9]\b").expect("credit-card regex is valid")
    });

    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for candidate in re.find_iter(text) {
        let raw = candidate.as_str();
        let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
        if !(13..=19).contains(&digits.len()) || digits.starts_with('0') {
            continue;
        }
        if digits.chars().all(|c| c == digits.as_bytes()[0] as char) {
            continue;
        }
        // At most one separator between digits.
        let mut previous_separator = false;
        let mut malformed = false;
        for c in raw.chars() {
            let separator = matches!(c, ' ' | '-');
            if separator && previous_separator {
                malformed = true;
                break;
            }
            previous_separator = separator;
        }
        if malformed
            || !luhn_valid(&digits)
            || is_embedded_in_structured_value(text, candidate.start(), candidate.end())
        {
            continue;
        }
        result.push_str(&text[last_end..candidate.start()]);
        result.push_str(CREDIT_CARD_PLACEHOLDER);
        last_end = candidate.end();
    }
    result.push_str(&text[last_end..]);
    result
}

/// Whether an SSN-shaped string has valid SSA field ranges.
fn ssn_fields_valid(ssn: &str) -> bool {
    let parts: Vec<&str> = ssn.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    let area: u32 = parts[0].parse().unwrap_or(0);
    let group: u32 = parts[1].parse().unwrap_or(0);
    let serial: u32 = parts[2].parse().unwrap_or(0);
    area != 0 && area != 666 && area < 900 && group != 0 && serial != 0
}

/// Replace US Social Security numbers (`AAA-GG-SSSS`).
///
/// Rejects the SSA's never-issued ranges (`000`, `666`, `900+` area codes,
/// group `00`, serial `0000`) and any match embedded in a path or frame.
fn scrub_ssns(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"\b[0-9]{3}-[0-9]{2}-[0-9]{4}\b").expect("SSN regex is valid"));

    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for candidate in re.find_iter(text) {
        if !ssn_fields_valid(candidate.as_str())
            || is_embedded_in_structured_value(text, candidate.start(), candidate.end())
        {
            continue;
        }
        result.push_str(&text[last_end..candidate.start()]);
        result.push_str(SSN_PLACEHOLDER);
        last_end = candidate.end();
    }
    result.push_str(&text[last_end..]);
    result
}

/// Validate an IBAN with the ISO 13616 mod-97 checksum.
///
/// The candidate may contain single spaces between groups; they are removed
/// before validation. Only 15–34 alphanumeric characters after the country and
/// check digits are accepted, which keeps ordinary identifiers out.
fn iban_valid(candidate: &str) -> bool {
    let cleaned: String = candidate.chars().filter(|c| *c != ' ').collect();
    if !(15..=34).contains(&cleaned.len())
        || !cleaned.chars().all(|c| c.is_ascii_alphanumeric())
        || !cleaned[..2].chars().all(|c| c.is_ascii_uppercase())
        || !cleaned[2..4].chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }

    // Move the first four characters to the end, then fold letters to 10..35
    // and take the remainder mod 97 as we go.
    let rearranged = format!("{}{}", &cleaned[4..], &cleaned[..4]);
    let mut remainder: u32 = 0;
    for byte in rearranged.bytes() {
        if byte.is_ascii_digit() {
            remainder = (remainder * 10 + u32::from(byte - b'0')) % 97;
        } else {
            let value = u32::from(byte - b'A') + 10;
            remainder = (remainder * 100 + value) % 97;
        }
    }
    remainder == 1
}

/// Replace checksum-valid IBANs.
///
/// The shape alone is not enough — an uppercase identifier can look like an
/// IBAN — so the mod-97 checksum must pass. Matches embedded in paths or frames
/// are left alone.
fn scrub_ibans(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\b[A-Z]{2}[0-9]{2}(?: ?[A-Z0-9]{1,4}){3,8}\b").expect("IBAN regex is valid")
    });

    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for candidate in re.find_iter(text) {
        if !iban_valid(candidate.as_str())
            || is_embedded_in_structured_value(text, candidate.start(), candidate.end())
        {
            continue;
        }
        result.push_str(&text[last_end..candidate.start()]);
        result.push_str(IBAN_PLACEHOLDER);
        last_end = candidate.end();
    }
    result.push_str(&text[last_end..]);
    result
}

/// Scrubber that regex-replaces sensitive patterns.
#[derive(Debug, Clone)]
pub struct Scrubber {
    patterns: Vec<(Regex, String)>,
}

impl Default for Scrubber {
    fn default() -> Self {
        Self::new()
    }
}

impl Scrubber {
    /// Create a new scrubber with the default pattern set.
    pub fn new() -> Self {
        let mut patterns = Vec::new();

        // Email addresses
        if let Ok(re) = Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}") {
            patterns.push((re, "<EMAIL>".to_string()));
        }

        // IPv4 addresses
        if let Ok(re) = Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b") {
            patterns.push((re, "<IP>".to_string()));
        }

        // IPv6 addresses (simplified — matches common forms)
        if let Ok(re) = Regex::new(r"\b(?:[0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}\b") {
            patterns.push((re, "<IP>".to_string()));
        }

        // PEM private key blocks. The whole block goes first (the base64 body
        // is the key material); the header-only pattern is a fallback for a
        // truncated block that has no `END` line.
        if let Ok(re) =
            Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----")
        {
            patterns.push((re, "<PRIVATE_KEY>".to_string()));
        }
        if let Ok(re) = Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----") {
            patterns.push((re, "<PRIVATE_KEY>".to_string()));
        }

        // Bearer / authorization header values
        if let Ok(re) = Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}") {
            patterns.push((re, "<AUTH_TOKEN>".to_string()));
        }

        // JSON Web Tokens: an `eyJ` header and exactly three dot-separated
        // base64url segments.
        if let Ok(re) = Regex::new(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b") {
            patterns.push((re, "<JWT>".to_string()));
        }

        // GitHub tokens (ghp_, gho_, ghu_, ghs_, ghr_) and fine-grained PATs.
        if let Ok(re) = Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }
        if let Ok(re) = Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // AWS access key IDs: `AKIA` plus 16 alphanumerics.
        if let Ok(re) = Regex::new(r"\bAKIA[A-Za-z0-9]{16}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // Slack tokens
        if let Ok(re) = Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // Generic API keys / tokens (labelled, base64-like)
        if let Ok(re) = Regex::new(
            r#"\b(?:sk-|api[_-]?key|token|secret)[_-]?[=:]?\s*["']?[a-zA-Z0-9_\-]{20,}["']?"#,
        ) {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // Home directory paths — attempt to catch /home/username or /Users/username
        if let Ok(re) = Regex::new(r"(/(?:home|Users)/)[^/\s]+") {
            patterns.push((re, "${1}<USER>".to_string()));
        }

        Self { patterns }
    }

    /// Scrub sensitive data from a string, returning the sanitized version.
    ///
    /// The named patterns run first (so labelled credentials report their
    /// specific placeholder), then any remaining long high-entropy run is
    /// replaced with [`SECRET_PLACEHOLDER`].
    pub fn scrub(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (re, replacement) in &self.patterns {
            result = re.replace_all(&result, replacement.as_str()).to_string();
        }
        // Checksum-validated detectors run after the shape-based patterns and
        // before the generic high-entropy scan.
        result = scrub_credit_cards(&result);
        result = scrub_ssns(&result);
        result = scrub_ibans(&result);
        scrub_high_entropy_tokens(&result)
    }

    /// Recursively scrub a JSON value in place.
    ///
    /// Model-name fields (see [`is_model_key`]) are redacted to
    /// [`CONFIDENTIAL_MODEL`] first. Then strings go through the same pattern
    /// set as [`Self::scrub`], and object keys on the [`EXCLUDED_FIELDS`] list
    /// are *removed* (not blanked), so the field never reaches the wire even if
    /// it slipped through event-level scrubbing.
    pub fn scrub_json(&self, value: &mut serde_json::Value) {
        redact_model_fields(value);
        self.scrub_patterns(value);
    }

    /// Apply the regex pattern set and drop excluded fields, recursively.
    ///
    /// Separated from [`Self::scrub_json`] so model redaction and pattern
    /// scrubbing stay independent.
    fn scrub_patterns(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => *s = self.scrub(s),
            serde_json::Value::Array(items) => {
                for item in items.iter_mut() {
                    self.scrub_patterns(item);
                }
            }
            serde_json::Value::Object(map) => {
                map.retain(|key, _| !is_excluded_field(key));
                for (_, child) in map.iter_mut() {
                    self.scrub_patterns(child);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    /// Scrub a [`TelemetryEvent`] in place.
    pub fn scrub_event(&self, event: &mut crate::TelemetryEvent) {
        use crate::TelemetryEvent;
        match event {
            TelemetryEvent::Error { message, .. } => {
                *message = self.scrub(message);
            }
            TelemetryEvent::ApiCall {
                provider, model, ..
            } => {
                *provider = self.scrub(provider);
                *model = self.scrub(model);
            }
            TelemetryEvent::ToolUsed { tool_name, .. } => {
                *tool_name = self.scrub(tool_name);
            }
            TelemetryEvent::SessionStart { .. } | TelemetryEvent::SessionEnd { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_email() {
        let scrubber = Scrubber::new();
        let text = "Contact alice@123.com for help";
        assert_eq!(scrubber.scrub(text), "Contact <EMAIL> for help");
    }

    #[test]
    fn scrub_ipv4() {
        let scrubber = Scrubber::new();
        let text = "Server at 192.168.1.1 responded";
        assert_eq!(scrubber.scrub(text), "Server at <IP> responded");
    }

    #[test]
    fn scrub_home_path() {
        let scrubber = Scrubber::new();
        let text = "File at /home/alice/project/src/main.rs";
        assert_eq!(
            scrubber.scrub(text),
            "File at /home/<USER>/project/src/main.rs"
        );
    }

    #[test]
    fn scrub_api_key() {
        let scrubber = Scrubber::new();
        let text = "api_key=sk-abc123def456ghi789jkl012mno345";
        assert_eq!(scrubber.scrub(text), "<API_KEY>");
    }

    #[test]
    fn scrub_event_error_message() {
        let scrubber = Scrubber::new();
        let mut event = crate::TelemetryEvent::Error {
            error_type: "io".into(),
            message: "failed to read /home/bob/.ssh/id_rsa for bob@123.com".into(),
            timestamp: chrono::Utc::now(),
        };
        scrubber.scrub_event(&mut event);
        if let crate::TelemetryEvent::Error { message, .. } = event {
            assert!(!message.contains("bob@123.com"));
            assert!(message.contains("<EMAIL>"));
            assert!(message.contains("<USER>"));
        }
    }

    #[test]
    fn scrub_github_token() {
        let scrubber = Scrubber::new();
        let text = "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 used";
        let scrubbed = scrubber.scrub(text);
        assert!(!scrubbed.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"));
        assert!(scrubbed.contains("<API_KEY>"));
    }

    #[test]
    fn scrub_aws_access_key() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("key AKIAIOSFODNN7EXAMPLE here");
        assert!(!scrubbed.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(scrubbed.contains("<API_KEY>"));
    }

    #[test]
    fn scrub_bearer_token() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("Authorization: Bearer abc123DEF456ghi789");
        assert!(!scrubbed.contains("abc123DEF456ghi789"));
        assert!(scrubbed.contains("<AUTH_TOKEN>"));
    }

    #[test]
    fn scrub_private_key_header() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("-----BEGIN RSA PRIVATE KEY-----\nMIIE...");
        assert!(scrubbed.contains("<PRIVATE_KEY>"));
    }

    #[test]
    fn scrub_jwt() {
        let scrubber = Scrubber::new();
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let scrubbed = scrubber.scrub(&format!("token={token}"));
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains("<JWT>"));
    }

    #[test]
    fn excluded_field_match_is_case_insensitive() {
        assert!(is_excluded_field("api_key"));
        assert!(is_excluded_field("API_KEY"));
        assert!(is_excluded_field("Authorization"));
        assert!(!is_excluded_field("model"));
    }

    /// The exclusion set drops sensitive *keys* outright — the field must not
    /// appear on the wire even as an empty string.
    #[test]
    fn scrub_json_drops_excluded_fields() {
        let scrubber = Scrubber::new();
        let mut value = serde_json::json!({
            "provider": "anthropic",
            "api_key": "sk-abcdefghijklmnopqrstuvwxyz",
            "nested": { "Authorization": "Bearer xyz", "keep": 1 }
        });
        scrubber.scrub_json(&mut value);

        assert!(value.get("api_key").is_none());
        assert!(value["nested"].get("Authorization").is_none());
        // A non-model, non-excluded field survives untouched.
        assert_eq!(value["provider"], serde_json::json!("anthropic"));
        assert_eq!(value["nested"]["keep"], serde_json::json!(1));
    }

    // ── Model-name redaction ─────────────────────────────────────────────

    /// Every shape the model-key regex must match, plus the always-model keys.
    #[test]
    fn model_key_regex_matches_every_shape() {
        for key in [
            "model",
            "model_id",
            "fast_model",
            "fast_models",
            "fastModels",
            "fastModel",
            "FastModels",
        ] {
            assert!(is_model_key(key), "{key} must be a model field");
        }
        for key in ["chain_entry", "fallbackFrom", "shortName"] {
            assert!(is_model_key(key), "{key} is always a model field");
        }
    }

    /// Keys that must not be treated as model fields.
    #[test]
    fn model_key_regex_rejects_non_model_keys() {
        for key in [
            "provider",
            "message",
            "session_id",
            "username",
            "repository_model",
        ] {
            assert!(!is_model_key(key), "{key} must not be a model field");
        }
    }

    /// The explicit exclusion set: `repository_model` matches the model-key
    /// regex, so only the exclusion entry keeps it in the clear.
    #[test]
    fn excluded_model_key_is_not_redacted() {
        assert!(model_key_regex().is_match("repository_model"));
        assert!(!is_model_key("repository_model"));

        let mut value = serde_json::json!({ "repository_model": "claude-opus-4" });
        redact_model_fields(&mut value);
        assert_eq!(
            value["repository_model"],
            serde_json::json!("claude-opus-4")
        );
    }

    /// A confidential model name is replaced in every model-shaped key of a
    /// nested payload; a placeholder is left alone.
    #[test]
    fn confidential_model_is_replaced_in_every_nested_model_key() {
        let scrubber = Scrubber::new();
        let mut value = serde_json::json!({
            "type": "api_call",
            "model": "claude-opus-4-6",
            "nested": {
                "fallback_models": "claude-sonnet-4-6",
                "chain_entry": "internal-codename-7",
                "items": [{ "shortName": "another-internal-model" }]
            },
            "repository_model": "claude-haiku-4-5"
        });
        scrubber.scrub_json(&mut value);

        assert_eq!(value["model"], serde_json::json!(CONFIDENTIAL_MODEL));
        assert_eq!(
            value["nested"]["fallback_models"],
            serde_json::json!(CONFIDENTIAL_MODEL)
        );
        assert_eq!(
            value["nested"]["chain_entry"],
            serde_json::json!(CONFIDENTIAL_MODEL)
        );
        assert_eq!(
            value["nested"]["items"][0]["shortName"],
            serde_json::json!(CONFIDENTIAL_MODEL)
        );
        // The exclusion set keeps this one verbatim.
        assert_eq!(
            value["repository_model"],
            serde_json::json!("claude-haiku-4-5")
        );

        let rendered = value.to_string();
        for secret in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "internal-codename-7",
            "another-internal-model",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        assert!(rendered.contains(CONFIDENTIAL_MODEL));
    }

    /// Placeholders name no model, so they are never replaced.
    #[test]
    fn model_placeholders_are_left_alone() {
        let scrubber = Scrubber::new();
        let mut value = serde_json::json!({
            "model": "inherit",
            "model_id": "unknown",
            "fallbackFrom": "default",
            "shortName": "none",
            "other_model": "nonconforming",
            "repository_model": "concrete-model"
        });
        scrubber.scrub_json(&mut value);

        assert_eq!(value["model"], serde_json::json!("inherit"));
        assert_eq!(value["model_id"], serde_json::json!("unknown"));
        assert_eq!(value["fallbackFrom"], serde_json::json!("default"));
        assert_eq!(value["shortName"], serde_json::json!("none"));
        assert_eq!(value["other_model"], serde_json::json!("nonconforming"));
        assert_eq!(
            value["repository_model"],
            serde_json::json!("concrete-model")
        );
    }

    /// An unavailable model (empty string / non-string) becomes confidential.
    #[test]
    fn unavailable_model_becomes_confidential() {
        let mut value = serde_json::json!({ "model": "", "model_id": null });
        redact_model_fields(&mut value);
        assert_eq!(value["model"], serde_json::json!(CONFIDENTIAL_MODEL));
        assert_eq!(value["model_id"], serde_json::json!(CONFIDENTIAL_MODEL));
    }

    // ── Strict (confidentiality) mode ────────────────────────────────────

    /// A payload built only from allowlisted fields passes unmarked.
    #[test]
    fn allowlisted_payload_passes_strict_mode() {
        let mut value = serde_json::json!({
            "type": "api_call",
            "provider": "anthropic",
            "model": "confidential",
            "tokens_in": 10,
            "tokens_out": 20,
            "timestamp": "2026-01-01T00:00:00Z"
        });
        assert!(strictly_allowlisted(&value));
        assert!(
            !mark_unredacted_if_unallowlisted(&mut value),
            "an allowlisted payload must not be marked"
        );
        assert!(value.get(UNREDACTED_MARKER_KEY).is_none());
    }

    /// A payload carrying an unknown field cannot be validated, so it is marked
    /// unredacted — the marker travels in the payload itself.
    #[test]
    fn unallowlisted_payload_is_marked_unredacted() {
        let mut value = serde_json::json!({
            "type": "error",
            "message": "something failed",
            "conversation_snippet": "user said a secret thing"
        });
        assert!(!strictly_allowlisted(&value));
        assert!(mark_unredacted_if_unallowlisted(&mut value));
        assert_eq!(
            value[UNREDACTED_MARKER_KEY],
            serde_json::json!(UNREDACTED_MARKER_VALUE)
        );
    }

    /// The allowlist is recursive: a known top-level field with an unknown
    /// nested field still fails validation.
    #[test]
    fn strict_mode_rejects_unknown_nested_field() {
        let mut value = serde_json::json!({
            "type": "api_call",
            "nested": { "surprise": true }
        });
        assert!(!strictly_allowlisted(&value));
        assert!(mark_unredacted_if_unallowlisted(&mut value));
        assert_eq!(
            value[UNREDACTED_MARKER_KEY],
            serde_json::json!(UNREDACTED_MARKER_VALUE)
        );
    }

    #[test]
    fn scrub_json_scrubs_nested_strings() {
        let scrubber = Scrubber::new();
        let mut value = serde_json::json!({
            "messages": ["mail alice@example.com", "at /Users/alice/project"],
        });
        scrubber.scrub_json(&mut value);
        let rendered = value.to_string();
        assert!(!rendered.contains("alice@example.com"));
        assert!(!rendered.contains("/Users/alice"));
        assert!(rendered.contains("<EMAIL>"));
        assert!(rendered.contains("<USER>"));
    }

    // ── High-entropy secret tokens ───────────────────────────────────────

    #[test]
    fn high_entropy_mixed_alphanumeric_token_is_redacted() {
        let scrubber = Scrubber::new();
        let token = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6";
        let scrubbed = scrubber.scrub(&format!("key was {token} here"));
        assert!(!scrubbed.contains(token), "{scrubbed}");
        assert_eq!(scrubbed, "key was <SECRET> here");
    }

    #[test]
    fn high_entropy_base64url_token_is_redacted() {
        let scrubber = Scrubber::new();
        let token = "dGhpcy1pcy1hLXNlY3JldC10b2tlbi12YWx1ZS0xMjM0";
        assert!(token.len() >= HIGH_ENTROPY_MIN_LEN);
        let scrubbed = scrubber.scrub(&format!("value={token}"));
        assert!(!scrubbed.contains(token), "{scrubbed}");
        assert!(scrubbed.contains(SECRET_PLACEHOLDER));
    }

    #[test]
    fn high_entropy_hex_digest_is_redacted() {
        let scrubber = Scrubber::new();
        let token = "0123456789abcdef0123456789abcdef01234567";
        let scrubbed = scrubber.scrub(&format!("sha1 {token}"));
        assert!(!scrubbed.contains(token), "{scrubbed}");
        assert!(scrubbed.contains(SECRET_PLACEHOLDER));
    }

    #[test]
    fn repeated_low_entropy_run_is_not_a_secret() {
        let scrubber = Scrubber::new();
        // Mixed character classes, but far too repetitive to be a secret.
        let token = "AbcAbcAbcAbcAbcAbcAbcAbcAbcAbc123Abc";
        let scrubbed = scrubber.scrub(&format!("payload {token}"));
        assert!(scrubbed.contains(token), "{scrubbed}");
    }

    /// File paths, Rust symbols, backtrace frames and URLs are diagnostics, not
    /// secrets. The detector must not touch them.
    #[test]
    fn diagnostic_shapes_survive_high_entropy_scrubbing() {
        let scrubber = Scrubber::new();
        for text in [
            "at /rustc/abcdef0123456789abcdef0123456789/library/std/src/panicking.rs:123:5",
            "coderex_core::engine::QueryEngine::submit",
            "at file.rs:12:9",
            "this_is_a_very_long_function_name_with_digits_12345",
            "ThisIsAVeryLongCamelCaseTypeNameWithoutDigits",
            "_ZN4core3fmt9Formatter3pad17h0123456789abcdefE",
            "https://example.com/docs/abcdefghijklmnopqrstuvwxyz012345",
            "The quick brown fox jumps over the lazy dog and keeps running.",
        ] {
            let scrubbed = scrubber.scrub(text);
            assert!(
                !scrubbed.contains(SECRET_PLACEHOLDER),
                "{text} -> {scrubbed}"
            );
        }
    }

    /// A hashed path component is part of a path, not a standalone token.
    #[test]
    fn long_hashed_path_component_survives() {
        let scrubber = Scrubber::new();
        let text = "/repo/.git/objects/ab/0123456789abcdef0123456789abcdef01234567";
        assert_eq!(scrubber.scrub(text), text);
    }

    /// The labelled-credential patterns run first, so a recognized key keeps its
    /// specific placeholder rather than the generic one.
    #[test]
    fn labelled_key_keeps_its_specific_placeholder() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("api_key=sk-abc123def456ghi789jkl012mno345");
        assert_eq!(scrubbed, "<API_KEY>");
    }

    // ── Credential families: true positives ──────────────────────────────

    #[test]
    fn jwt_with_three_segments_is_redacted() {
        let scrubber = Scrubber::new();
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2lnbmF0dXJl";
        let scrubbed = scrubber.scrub(&format!("Authorization: {token}"));
        assert!(!scrubbed.contains(token), "{scrubbed}");
        assert!(scrubbed.contains("<JWT>"), "{scrubbed}");
    }

    #[test]
    fn pem_private_key_block_is_redacted_in_full() {
        let scrubber = Scrubber::new();
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAsecretbody\n-----END RSA PRIVATE KEY-----";
        let scrubbed = scrubber.scrub(&format!("key follows:\n{pem}\ndone"));
        assert!(
            !scrubbed.contains("MIIEowIBAAKCAQEAsecretbody"),
            "{scrubbed}"
        );
        assert_eq!(scrubbed, "key follows:\n<PRIVATE_KEY>\ndone");
    }

    #[test]
    fn aws_access_key_id_is_redacted() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("key AKIAIOSFODNN7EXAMPLE here");
        assert!(!scrubbed.contains("AKIAIOSFODNN7EXAMPLE"), "{scrubbed}");
        assert!(scrubbed.contains("<API_KEY>"), "{scrubbed}");
    }

    #[test]
    fn github_tokens_are_redacted() {
        let scrubber = Scrubber::new();
        for token in [
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz",
        ] {
            let scrubbed = scrubber.scrub(&format!("token {token} used"));
            assert!(!scrubbed.contains(token), "{scrubbed}");
            assert!(scrubbed.contains("<API_KEY>"), "{scrubbed}");
        }
    }

    #[test]
    fn slack_tokens_are_redacted() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("hook xoxb-1234567890-ABCDEFGHIJ sent");
        assert!(
            !scrubbed.contains("xoxb-1234567890-ABCDEFGHIJ"),
            "{scrubbed}"
        );
        assert!(scrubbed.contains("<API_KEY>"), "{scrubbed}");
    }

    #[test]
    fn bearer_values_are_redacted() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("Authorization: Bearer abc123DEF456ghi789");
        assert!(!scrubbed.contains("abc123DEF456ghi789"), "{scrubbed}");
        assert!(scrubbed.contains("<AUTH_TOKEN>"), "{scrubbed}");
    }

    #[test]
    fn luhn_valid_card_numbers_are_redacted() {
        let scrubber = Scrubber::new();
        for card in [
            "4111111111111111",
            "4111 1111 1111 1111",
            "4111-1111-1111-1111",
            "5500005555555559",
        ] {
            let scrubbed = scrubber.scrub(&format!("charged card {card} today"));
            assert!(!scrubbed.contains(card), "{scrubbed}");
            assert!(scrubbed.contains(CREDIT_CARD_PLACEHOLDER), "{scrubbed}");
        }
    }

    #[test]
    fn valid_us_ssns_are_redacted() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("ssn 123-45-6789 on file");
        assert!(!scrubbed.contains("123-45-6789"), "{scrubbed}");
        assert!(scrubbed.contains(SSN_PLACEHOLDER), "{scrubbed}");
    }

    #[test]
    fn valid_ibans_are_redacted() {
        let scrubber = Scrubber::new();
        for iban in ["GB82WEST12345698765432", "GB82 WEST 1234 5698 7654 32"] {
            let scrubbed = scrubber.scrub(&format!("account {iban} active"));
            assert!(!scrubbed.contains(iban), "{scrubbed}");
            assert!(scrubbed.contains(IBAN_PLACEHOLDER), "{scrubbed}");
        }
    }

    // ── Credential families: false positives ─────────────────────────────

    #[test]
    fn a_jwt_prefix_without_three_segments_survives() {
        let scrubber = Scrubber::new();
        let text = "the header eyJhbGciOiJIUzI1NiJ9 alone is not a JWT";
        assert_eq!(scrubber.scrub(text), text);
    }

    #[test]
    fn non_private_pem_headers_survive() {
        let scrubber = Scrubber::new();
        let text = "-----BEGIN CERTIFICATE----- is a public certificate";
        assert_eq!(scrubber.scrub(text), text);
    }

    #[test]
    fn short_or_non_akia_aws_lookalikes_survive() {
        let scrubber = Scrubber::new();
        for text in ["AKIA1234 is too short", "ASIAIOSFODNN7EXAMPLE is not AKIA"] {
            assert_eq!(scrubber.scrub(text), text);
        }
    }

    #[test]
    fn short_github_lookalikes_survive() {
        let scrubber = Scrubber::new();
        for text in ["ghp_shorttoken", "github_pat_short"] {
            assert_eq!(scrubber.scrub(text), text);
        }
    }

    #[test]
    fn short_slack_lookalikes_survive() {
        let scrubber = Scrubber::new();
        let text = "xoxb-short";
        assert_eq!(scrubber.scrub(text), text);
    }

    #[test]
    fn bearer_prose_survives() {
        let scrubber = Scrubber::new();
        let text = "a bearer of good news arrived";
        assert_eq!(scrubber.scrub(text), text);
    }

    /// The card detector is checksum- and context-aware: a card-like run inside
    /// a path, a filename, a URL or a file:line frame is not a payment card.
    #[test]
    fn card_like_runs_outside_payment_context_survive() {
        let scrubber = Scrubber::new();
        for text in [
            "/data/4111111111111111/report.json",
            "card-4111111111111111.txt",
            "https://example.com/4111111111111111",
            "src/open.rs:4111111111111111:12",
        ] {
            assert_eq!(scrubber.scrub(text), text, "{text}");
        }
    }

    #[test]
    fn numbers_failing_luhn_survive() {
        let scrubber = Scrubber::new();
        for text in ["order 1234567890123456 shipped", "id 0000000000000000"] {
            assert_eq!(scrubber.scrub(text), text, "{text}");
        }
    }

    #[test]
    fn invalid_ssn_ranges_and_paths_survive() {
        let scrubber = Scrubber::new();
        for text in [
            "placeholder 000-00-0000",
            "reserved 666-12-3456",
            "unassigned 900-12-3456",
            "/tmp/123-45-6789.txt",
        ] {
            assert_eq!(scrubber.scrub(text), text, "{text}");
        }
    }

    #[test]
    fn ibans_failing_the_checksum_survive() {
        let scrubber = Scrubber::new();
        for text in [
            "not an account GB00WEST12345698765432",
            "identifier AB12CDEFGHIJKLMNOP",
        ] {
            assert_eq!(scrubber.scrub(text), text, "{text}");
        }
    }

    /// The new detectors must not disturb diagnostics: paths, URLs, Rust
    /// symbols, frames and ordinary prose stay readable.
    #[test]
    fn diagnostics_survive_the_financial_detectors() {
        let scrubber = Scrubber::new();
        let text = "at /home/somebody/project/src/main.rs:123:5 coderex_core::engine::run https://example.com/docs/reference";
        let scrubbed = scrubber.scrub(text);
        assert_eq!(scrubbed.replace("/home/<USER>", "/home/somebody"), text);
    }
}
