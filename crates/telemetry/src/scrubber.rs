//! PII scrubbing for telemetry events.
//!
//! Sanitizes sensitive data from event payloads before storage.
//! Covers: email addresses, IP addresses, file paths containing usernames,
//! API keys and other credential shapes, an exclusion set of field names that
//! are dropped from payloads entirely, and model-name redaction (a dedicated
//! key regex plus an exclusion set). A stricter mode — enabled for
//! confidentiality-sensitive deployments — additionally validates a payload
//! against a field allowlist and marks anything it cannot validate.

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
pub const STRICT_ALLOWED_FIELDS: [&str; 14] = [
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

        // PEM private key blocks
        if let Ok(re) = Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----") {
            patterns.push((re, "<PRIVATE_KEY>".to_string()));
        }

        // Bearer / authorization header values
        if let Ok(re) = Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}") {
            patterns.push((re, "<AUTH_TOKEN>".to_string()));
        }

        // JSON Web Tokens (header.payload.signature)
        if let Ok(re) =
            Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
        {
            patterns.push((re, "<JWT>".to_string()));
        }

        // GitHub tokens (ghp_, gho_, ghu_, ghs_, ghr_)
        if let Ok(re) = Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // AWS access key IDs
        if let Ok(re) = Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b") {
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
    pub fn scrub(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (re, replacement) in &self.patterns {
            result = re.replace_all(&result, replacement.as_str()).to_string();
        }
        result
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
            TelemetryEvent::SessionStart { .. } => {}
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
}
